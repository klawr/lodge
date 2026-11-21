//! How a devcontainer's container is brought up.
//!
//! Two backends: compose (for `dockerComposeFile`) and a plain run of an `image`
//! or a locally built `Dockerfile`. Both are plain data; the engine they act on
//! is passed in. Everything after the container exists is shared and works off
//! its id.

use crate::output::info;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::create::{CreateSettings, Mount, parse_app_ports};
use super::spec::{CreateConfig, SourceConfig};
use super::vars::{Vars, path_tag};
use crate::host::HostRef;
use crate::runtime::{BuildSpec, Containers, RunSpec, Runtime};
use crate::shell::slug;

/// Label on every container lodge creates for a devcontainer: the project's
/// path on its host. `ldg ls` lists containers by it.
pub const LABEL_PROJECT: &str = "dev.lodge.project";

/// Brings a devcontainer's container up. Each backend only has to know how to
/// find an existing container and create/start one; everything after that —
/// hashing, env, features, lifecycle hooks — is shared and works off the id.
pub(super) trait Backend {
    /// Id of an existing container for this devcontainer (running or stopped),
    /// or None if it hasn't been created yet.
    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>>;
    /// Brings the container up and returns its id. `existing` is what
    /// [`existing`](Self::existing) found (a backend that must ask the engine
    /// again anyway, like compose, may ignore it); it is recreated from scratch
    /// when `recreate` is set. `extra` (what features ask for) applies when it
    /// is created.
    fn up(
        &self,
        rt: &dyn Runtime,
        existing: Option<&str>,
        recreate: bool,
        extra: &CreateSettings,
    ) -> Result<String>;
    /// Removes the container (and, for compose, the project's network).
    fn remove(&self, rt: &dyn Runtime) -> Result<()>;
    /// Whether it runs the container through compose, which `ldg doctor`
    /// requires then, and which owns the container user (no uid alignment).
    fn uses_compose(&self) -> bool;
}

pub(super) struct ComposeBackend {
    pub(super) dir: PathBuf,
    pub(super) files: Vec<String>,
    pub(super) service: String,
    // Explicit compose project name (-p). Without it, compose derives the
    // project from the compose file's directory — which is ".devcontainer"
    // for most devcontainers, so *every* such project on a daemon would share
    // one project name and lodge could adopt a different project's container.
    pub(super) project: String,
    // `runServices`: bring up only these (always including `service`). Empty
    // means every service in the compose file.
    pub(super) run_services: Vec<String>,
    /// The project's path, recorded as [`LABEL_PROJECT`] on the service.
    pub(super) project_path: PathBuf,
    /// The project's host, where compose runs and reads its files (and so
    /// where the override file is written).
    pub(super) host: HostRef,
}

impl Backend for ComposeBackend {
    fn remove(&self, rt: &dyn Runtime) -> Result<()> {
        rt.compose()?.down(&self.dir, &self.project, &self.files)
    }

    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>> {
        rt.compose()?.ps(
            &self.dir,
            &self.project,
            &self.files,
            &self.service,
            Containers::All,
        )
    }

    fn uses_compose(&self) -> bool {
        true
    }

    // Compose finds its own container (and must be asked again after `up`
    // anyway, to learn the id), so `existing` isn't needed.
    fn up(
        &self,
        rt: &dyn Runtime,
        _existing: Option<&str>,
        recreate: bool,
        extra: &CreateSettings,
    ) -> Result<String> {
        // On recreate, rebuild and force a fresh container; otherwise keep the
        // existing one (and stop compose recreating it on its own config-hash).
        let mode: &[&str] = if recreate {
            &["--build", "--force-recreate"]
        } else {
            &["--no-recreate"]
        };

        // lodge's label and the features' settings go in a temporary compose
        // override, appended as the last `-f` so it layers over the project's
        // files without changing the project identity (the first file still
        // sets the project dir). Compose reads it client-side, so it is
        // written on the project's host (where compose runs).
        let mut files = self.files.clone();
        let project_path = self.project_path.to_string_lossy();
        let content = extra.compose_override(&self.service, &[(LABEL_PROJECT, &project_path)]);
        let override_file = self
            .host
            .write_scratch("compose-override.yml", &content)
            .context("writing compose override")?;
        files.push(override_file.path().to_string());

        let mut up_args: Vec<&str> = mode.to_vec();
        up_args.extend(self.run_services.iter().map(String::as_str));
        let result = rt
            .compose()
            .and_then(|c| c.up(&self.dir, &self.project, &files, &up_args));
        drop(override_file);
        result?;

        rt.compose()?
            .ps(
                &self.dir,
                &self.project,
                &self.files,
                &self.service,
                Containers::Running,
            )?
            .with_context(|| {
                format!(
                    "could not find running container for service '{}'",
                    self.service
                )
            })
    }
}

pub(super) enum ImageSource {
    Image(String),
    Build {
        context: PathBuf,
        dockerfile: Option<String>,
        /// The build stage (`build.target` in devcontainer.json).
        stage: Option<String>,
        args: BTreeMap<String, String>,
        tag: String,
    },
}

// A plain (non-compose) devcontainer: an `image` to run or a `build` to build,
// run detached as `sleep infinity` with the workspace mounted, under a stable
// per-project name so lodge can find/recreate it.
pub(super) struct ContainerBackend {
    pub(super) name: String,
    pub(super) source: ImageSource,
    /// What every run of it has, before what features add and the image.
    pub(super) run: RunSpec,
    // `overrideCommand` (default true): keep the container alive with
    // `sleep infinity` instead of running the image's own command.
    pub(super) override_command: bool,
}

impl Backend for ContainerBackend {
    fn remove(&self, rt: &dyn Runtime) -> Result<()> {
        rt.remove(&self.name)
    }

    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>> {
        rt.find_by_name(&self.name)
    }

    fn uses_compose(&self) -> bool {
        false
    }

    fn up(
        &self,
        rt: &dyn Runtime,
        existing: Option<&str>,
        recreate: bool,
        extra: &CreateSettings,
    ) -> Result<String> {
        let exists = existing.is_some();
        if exists && !recreate {
            info!("starting {}", self.name);
            rt.start(&self.name)
                .context("starting the existing container")?;
            return self.id(rt);
        }
        if exists {
            rt.remove(&self.name)
                .context("removing the old container before recreating it")?;
        }

        let image = self.resolve_image(rt)?;
        info!("creating container {}", self.name);
        let mut spec = self.run.clone();
        extra.apply(&mut spec);
        spec.image = image;
        if self.override_command {
            spec.command = vec!["sleep".into(), "infinity".into()];
        }
        rt.run_detached(&spec).context("creating container")?;
        self.id(rt)
    }
}

impl ContainerBackend {
    /// The engine's id for the container, the same form `existing` returns, so
    /// everything keyed by it (the forward daemon) agrees across commands.
    fn id(&self, rt: &dyn Runtime) -> Result<String> {
        rt.find_by_name(&self.name)?
            .with_context(|| format!("container {} vanished after starting it", self.name))
    }

    fn resolve_image(&self, rt: &dyn Runtime) -> Result<String> {
        match &self.source {
            ImageSource::Image(img) => Ok(img.clone()),
            ImageSource::Build {
                context,
                dockerfile,
                stage,
                args,
                tag,
            } => {
                info!("building image {tag}");
                rt.build_image(&BuildSpec {
                    context: context.clone(),
                    dockerfile: dockerfile.clone(),
                    stage: stage.clone(),
                    args: args.clone(),
                    tag: tag.clone(),
                })?;
                Ok(tag.clone())
            }
        }
    }
}

// What a ContainerBackend asks the engine for: the workspace mount (or a
// default bind), the working dir, the settings that devcontainer.json states
// directly (its `mounts` among them), and the user's `runArgs`, which the engine
// passes through last so they can override.
fn build_run_spec(
    name: &str,
    user_args: Option<Vec<String>>,
    workspace_mount: Option<String>,
    settings: &CreateSettings,
    vars: &Vars,
    project_path: &Path,
    workspace_folder: &str,
) -> RunSpec {
    let mut spec = RunSpec {
        name: name.to_string(),
        labels: vec![(
            LABEL_PROJECT.to_string(),
            project_path.display().to_string(),
        )],
        workdir: Some(workspace_folder.to_string()),
        ..RunSpec::default()
    };
    match workspace_mount {
        Some(m) => spec.mounts.push(vars.apply(&m)),
        None => spec.binds.push((
            project_path.display().to_string(),
            workspace_folder.to_string(),
        )),
    }
    settings.apply(&mut spec);
    spec.engine_args = user_args
        .unwrap_or_default()
        .into_iter()
        .map(|a| vars.apply(&a))
        .collect();
    spec
}

// ── construction from devcontainer.json ────────────────────────────────────────

/// What backend construction needs to know about the project.
pub(super) struct BackendContext<'a> {
    pub(super) project_path: &'a Path,
    /// The project's host.
    pub(super) host: &'a HostRef,
    pub(super) base_dir: &'a Path,
    pub(super) workspace_folder: &'a str,
    pub(super) vars: &'a Vars<'a>,
}

pub(super) struct BuiltBackend {
    pub(super) backend: Box<dyn Backend>,
    /// Files besides the config (the Dockerfile, the compose files) to fold into the
    /// drift hash.
    pub(super) hash_extra: Vec<PathBuf>,
}

/// Picks the backend for whichever form the devcontainer uses.
pub(super) fn build_backend(
    source: SourceConfig,
    create: CreateConfig,
    container_user: Option<String>,
    ctx: &BackendContext,
) -> Result<BuiltBackend> {
    if source.docker_compose_file.is_some() {
        compose_backend(source, ctx)
    } else if source.image.is_some() || source.build.is_some() || source.docker_file.is_some() {
        Ok(container_backend(source, create, container_user, ctx))
    } else {
        bail!("devcontainer.json must set one of: dockerComposeFile, image, build, or dockerFile")
    }
}

fn compose_backend(source: SourceConfig, ctx: &BackendContext) -> Result<BuiltBackend> {
    let files = match source.docker_compose_file {
        Some(serde_json::Value::String(s)) => vec![s],
        Some(serde_json::Value::Array(a)) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => bail!("unexpected dockerComposeFile format"),
    };
    let service = source
        .service
        .context("devcontainer.json with dockerComposeFile is missing 'service'")?;
    let mut run_services = source.run_services.unwrap_or_default();
    if !run_services.is_empty() && !run_services.contains(&service) {
        run_services.push(service.clone());
    }
    let hash_extra = files.iter().map(|f| ctx.base_dir.join(f)).collect();
    Ok(BuiltBackend {
        backend: Box::new(ComposeBackend {
            dir: ctx.base_dir.to_path_buf(),
            files,
            service,
            run_services,
            // Compose project names must be lowercase.
            project: container_name(ctx.project_path).to_lowercase(),
            project_path: ctx.project_path.to_path_buf(),
            host: ctx.host.clone(),
        }),
        hash_extra,
    })
}

fn container_backend(
    source: SourceConfig,
    create: CreateConfig,
    container_user: Option<String>,
    ctx: &BackendContext,
) -> BuiltBackend {
    let name = container_name(ctx.project_path);
    let image_source = image_source(source, &name, ctx.base_dir);
    let settings = CreateSettings {
        privileged: create.privileged.unwrap_or(false),
        init: create.init.unwrap_or(false),
        cap_add: create.cap_add.unwrap_or_default(),
        security_opt: create.security_opt.unwrap_or_default(),
        mounts: create
            .mounts
            .unwrap_or_default()
            .iter()
            .filter_map(|m| Mount::parse(m, &|s| ctx.vars.apply(s)))
            .collect(),
        container_user,
        published_ports: parse_app_ports(create.app_port),
    };
    let run = build_run_spec(
        &name,
        create.run_args,
        create.workspace_mount,
        &settings,
        ctx.vars,
        ctx.project_path,
        ctx.workspace_folder,
    );
    // The Dockerfile counts toward drift even when it lives outside the hashed
    // folder (`"dockerfile": "../Dockerfile"`), like compose files do.
    let hash_extra = match &image_source {
        ImageSource::Build {
            context,
            dockerfile,
            ..
        } => vec![context.join(dockerfile.as_deref().unwrap_or("Dockerfile"))],
        ImageSource::Image(_) => vec![],
    };
    BuiltBackend {
        backend: Box::new(ContainerBackend {
            name,
            source: image_source,
            run,
            override_command: create.override_command.unwrap_or(true),
        }),
        hash_extra,
    }
}

/// An `image` to run, or a `build` to build (with `dockerFile`/`context` as the
/// older spellings of its fields).
fn image_source(source: SourceConfig, container_name: &str, base_dir: &Path) -> ImageSource {
    if let Some(image) = source.image {
        return ImageSource::Image(image);
    }
    let build = source.build.unwrap_or_default();
    let context = build
        .context
        .or(source.context)
        .unwrap_or_else(|| ".".to_string());
    ImageSource::Build {
        context: base_dir.join(context),
        dockerfile: build.dockerfile.or(source.docker_file),
        stage: build.target,
        args: build.args,
        tag: format!("{container_name}-img"),
    }
}

// A stable container name for a non-compose devcontainer: project basename plus
// a short hash of its path, so distinct projects with the same basename don't
// collide.
pub(super) fn container_name(project_path: &Path) -> String {
    let base = slug(&project_basename(project_path));
    format!("lodge-{base}-{}", path_tag(project_path))
}

/// The project folder's name, or `workspace` for `/`.
pub(super) fn project_basename(project_path: &Path) -> String {
    project_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::local;

    #[test]
    fn the_run_spec_has_the_workspace_the_settings_and_the_user_args_last() {
        let p = Path::new("/home/me/proj");
        let spec = build_run_spec(
            "lodge-proj",
            Some(vec!["--cap-drop=ALL".into()]),
            None,
            &CreateSettings {
                privileged: true,
                ..Default::default()
            },
            &Vars::new(p),
            p,
            "/workspaces/proj",
        );
        assert_eq!(
            spec,
            RunSpec {
                name: "lodge-proj".into(),
                labels: vec![("dev.lodge.project".into(), "/home/me/proj".into())],
                workdir: Some("/workspaces/proj".into()),
                binds: vec![("/home/me/proj".into(), "/workspaces/proj".into())],
                privileged: true,
                engine_args: vec!["--cap-drop=ALL".into()],
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_container_is_created_from_the_spec_plus_what_features_add_and_kept_alive() {
        use crate::runtime::fake::{FakeEnv, FakeRuntime};
        let rt = FakeRuntime::new(FakeEnv::new);
        let backend = |override_command| ContainerBackend {
            name: "lodge-p".into(),
            source: ImageSource::Image("img:1".into()),
            run: RunSpec {
                name: "lodge-p".into(),
                cap_add: vec!["A".into()],
                ..Default::default()
            },
            override_command,
        };
        let features = CreateSettings {
            cap_add: vec!["B".into()],
            ..Default::default()
        };
        backend(true).up(&rt, None, false, &features).unwrap();
        let spec = &rt.runs()[0];
        assert_eq!(spec.image, "img:1");
        assert_eq!(spec.cap_add, ["A", "B"]);
        assert_eq!(spec.command, ["sleep", "infinity"]);

        backend(false)
            .up(&rt, Some("lodge-p"), true, &features)
            .unwrap();
        assert!(rt.runs()[1].command.is_empty(), "the image's own command");
    }

    #[test]
    fn a_workspace_mount_replaces_the_default_bind() {
        let p = Path::new("/home/me/proj");
        let spec = build_run_spec(
            "n",
            None,
            Some("type=bind,source=${localWorkspaceFolder},target=/w".into()),
            &CreateSettings::default(),
            &Vars::new(p),
            p,
            "/w",
        );
        assert!(spec.binds.is_empty());
        assert_eq!(spec.mounts, ["type=bind,source=/home/me/proj,target=/w"]);
    }

    #[test]
    fn a_dockerfile_build_resolves_relative_to_the_config_directory() {
        let source = SourceConfig {
            docker_file: Some("Dockerfile".into()),
            context: Some("..".into()),
            ..Default::default()
        };
        match image_source(source, "lodge-proj-1", Path::new("/p/.devcontainer")) {
            ImageSource::Build {
                context,
                dockerfile,
                tag,
                ..
            } => {
                assert_eq!(context, PathBuf::from("/p/.devcontainer/.."));
                assert_eq!(dockerfile.as_deref(), Some("Dockerfile"));
                assert_eq!(tag, "lodge-proj-1-img");
            }
            ImageSource::Image(_) => panic!("expected a build"),
        }
    }

    #[test]
    fn container_name_is_stable_and_path_scoped() {
        let a = container_name(Path::new("/home/me/proj"));
        assert_eq!(a, container_name(Path::new("/home/me/proj")));
        assert!(a.starts_with("lodge-proj-"), "got {a}");
        // same basename, different path → different name (no collision)
        assert_ne!(a, container_name(Path::new("/elsewhere/proj")));
    }

    #[test]
    fn a_dockerfile_outside_the_config_folder_feeds_the_drift_hash() {
        let p = Path::new("/home/me/proj");
        let base = p.join(".devcontainer");
        let vars = Vars::new(p).with_container_workspace("/workspaces/proj");
        let ctx = BackendContext {
            project_path: p,
            host: &local(),
            base_dir: &base,
            workspace_folder: "/workspaces/proj",
            vars: &vars,
        };
        let build = |source: SourceConfig| {
            build_backend(source, CreateConfig::default(), None, &ctx)
                .unwrap()
                .hash_extra
        };
        assert_eq!(
            build(SourceConfig {
                docker_file: Some("Dockerfile".into()),
                context: Some("..".into()),
                ..Default::default()
            }),
            [PathBuf::from("/home/me/proj/.devcontainer/../Dockerfile")]
        );
        assert_eq!(
            build(SourceConfig {
                image: Some("debian".into()),
                ..Default::default()
            }),
            Vec::<PathBuf>::new()
        );
    }
}
