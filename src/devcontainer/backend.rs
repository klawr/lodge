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

use super::config::{CreateConfig, SourceConfig};
use super::vars::{Vars, path_tag};
use crate::runtime::{BuildSpec, Containers, Runtime};
use crate::shell::slug;
use crate::target::Target;

/// Label on every container lodge creates for a devcontainer: the project's
/// path on its host. `ldg ls` lists containers by it.
pub const LABEL_PROJECT: &str = "dev.lodge.project";

// Brings a devcontainer's container up. Each backend only has to know how to
// find an existing container and create/start one; everything after that —
// hashing, env, features, lifecycle hooks — is shared and works off the id.
pub(super) trait Backend {
    /// Id of an existing container for this devcontainer (running or stopped),
    /// or None if it hasn't been created yet.
    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>>;
    /// Brings the container up and returns its id. `existing` is what
    /// [`existing`](Self::existing) found; it is recreated from scratch when
    /// `recreate` is set. `extra` (what features ask for) applies when it is
    /// created. `target` is the project's host, where the engine reads files.
    fn up(
        &self,
        rt: &dyn Runtime,
        target: &Target,
        existing: Option<&str>,
        recreate: bool,
        extra: &CreateSettings,
    ) -> Result<String>;
    /// Removes the container (and, for compose, the project's network).
    fn remove(&self, rt: &dyn Runtime) -> Result<()>;
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

    // Compose finds its own container; `existing` isn't needed.
    fn up(
        &self,
        rt: &dyn Runtime,
        target: &Target,
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
        // sets the project dir). Compose reads it client-side, so with a remote
        // target it is written on the remote host (where compose runs).
        let mut files = self.files.clone();
        let project_path = self.project_path.to_string_lossy();
        let content = extra.compose_override(&self.service, &[(LABEL_PROJECT, &project_path)]);
        let override_file = target
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
        target: Option<String>,
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
    pub(super) run_args: Vec<String>,
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

    fn up(
        &self,
        rt: &dyn Runtime,
        _target: &Target,
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
        let mut run_args = self.run_args.clone();
        run_args.extend(extra.to_args());
        let command: &[&str] = if self.override_command {
            &["sleep", "infinity"]
        } else {
            &[]
        };
        rt.run_detached(&self.name, &run_args, &image, command)
            .context("creating container")?;
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
                target,
                args,
                tag,
            } => {
                info!("building image {tag}");
                rt.build_image(&BuildSpec {
                    context: context.clone(),
                    dockerfile: dockerfile.clone(),
                    target: target.clone(),
                    args: args.clone(),
                    tag: tag.clone(),
                })?;
                Ok(tag.clone())
            }
        }
    }
}

/// How a container is created: what devcontainer.json states directly (for the
/// image/Dockerfile backend; with compose these belong in the compose file),
/// and what features ask for (either backend).
#[derive(Debug, Default)]
pub(super) struct CreateSettings {
    pub(super) privileged: bool,
    pub(super) init: bool,
    pub(super) cap_add: Vec<String>,
    pub(super) security_opt: Vec<String>,
    pub(super) mounts: Vec<Mount>,
    pub(super) container_user: Option<String>,
    /// `-p` values, from `appPort`.
    pub(super) published_ports: Vec<String>,
}

impl CreateSettings {
    /// Adds `other`'s settings to these.
    pub(super) fn merge(&mut self, other: CreateSettings) {
        self.privileged |= other.privileged;
        self.init |= other.init;
        self.cap_add.extend(other.cap_add);
        self.security_opt.extend(other.security_opt);
        self.mounts.extend(other.mounts);
        self.container_user = other.container_user.or(self.container_user.take());
        self.published_ports.extend(other.published_ports);
    }

    /// The `docker run` flags these settings stand for.
    pub(super) fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.privileged {
            args.push("--privileged".into());
        }
        if self.init {
            args.push("--init".into());
        }
        for cap in &self.cap_add {
            args.extend(["--cap-add".into(), cap.clone()]);
        }
        for opt in &self.security_opt {
            args.extend(["--security-opt".into(), opt.clone()]);
        }
        for m in &self.mounts {
            args.extend(["--mount".into(), m.spec.clone()]);
        }
        if let Some(user) = &self.container_user {
            args.extend(["-u".into(), user.clone()]);
        }
        for port in &self.published_ports {
            args.extend(["-p".into(), port.clone()]);
        }
        args
    }

    /// A compose override adding these settings and `labels` to `service`.
    /// Every scalar is quoted, so no value can change the document's shape.
    /// `container_user` and `published_ports` are left out: they come from
    /// devcontainer.json, whose compose form configures them in the compose file.
    pub(super) fn compose_override(&self, service: &str, labels: &[(&str, &str)]) -> String {
        let q = yaml_string;
        let mut s = format!("services:\n  {}:\n", q(service));
        if self.privileged {
            s.push_str("    privileged: true\n");
        }
        if self.init {
            s.push_str("    init: true\n");
        }
        let mut list = |key: &str, items: Vec<String>| {
            if !items.is_empty() {
                s.push_str(&format!("    {key}:\n"));
                for item in items {
                    s.push_str(&format!("      - {}\n", q(&item)));
                }
            }
        };
        list("cap_add", self.cap_add.clone());
        list("security_opt", self.security_opt.clone());
        list(
            "volumes",
            self.mounts
                .iter()
                .filter_map(Mount::compose_volume)
                .collect(),
        );
        if !labels.is_empty() {
            s.push_str("    labels:\n");
            for (k, v) in labels {
                s.push_str(&format!("      {}: {}\n", q(k), q(v)));
            }
        }
        s
    }
}

/// A mount, from devcontainer.json's `mounts` or a feature's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mount {
    /// The `--mount` value.
    pub(super) spec: String,
    source: Option<String>,
    target: Option<String>,
}

impl Mount {
    /// Parses a mount in either spec form: a `--mount` string
    /// (`type=bind,source=/a,target=/b`) or a `{type, source, target}` object.
    /// `apply` substitutes variables in the values.
    pub(super) fn parse(value: &serde_json::Value, apply: &dyn Fn(&str) -> String) -> Option<Self> {
        match value {
            serde_json::Value::String(s) => {
                let spec = apply(s);
                let (mut source, mut target) = (None, None);
                for kv in spec.split(',') {
                    if let Some((k, v)) = kv.split_once('=') {
                        match k.trim() {
                            "source" | "src" => source = Some(v.to_string()),
                            "target" | "dst" | "destination" => target = Some(v.to_string()),
                            _ => {}
                        }
                    }
                }
                Some(Self {
                    spec,
                    source,
                    target,
                })
            }
            serde_json::Value::Object(o) => {
                let get = |k: &str| o.get(k).and_then(|v| v.as_str());
                let source = get("source").map(apply);
                let target = get("target").map(apply);
                let mut parts = Vec::new();
                if let Some(t) = get("type") {
                    parts.push(format!("type={t}"));
                }
                if let Some(s) = &source {
                    parts.push(format!("source={s}"));
                }
                if let Some(t) = &target {
                    parts.push(format!("target={t}"));
                }
                (!parts.is_empty()).then(|| Self {
                    spec: parts.join(","),
                    source,
                    target,
                })
            }
            _ => None,
        }
    }

    /// The compose `volumes` entry, `source:target`, when both are known.
    fn compose_volume(&self) -> Option<String> {
        Some(format!(
            "{}:{}",
            self.source.as_ref()?,
            self.target.as_ref()?
        ))
    }
}

/// A YAML double-quoted scalar (JSON string syntax is valid YAML).
fn yaml_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

// Parses `appPort`: a port, a "host:container" string, or an array of either.
// Published on the loopback interface only, so a devcontainer's ports are not
// exposed to the whole network.
fn parse_app_ports(value: Option<serde_json::Value>) -> Vec<String> {
    let items = match value {
        Some(serde_json::Value::Array(a)) => a,
        Some(v) => vec![v],
        None => vec![],
    };
    items
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_u64().map(|n| format!("127.0.0.1:{n}:{n}")),
            serde_json::Value::String(s) => Some(publish_spec(&s)),
            _ => None,
        })
        .collect()
}

fn publish_spec(s: &str) -> String {
    match s.split(':').count() {
        // "3000": the same port on both sides.
        1 => format!("127.0.0.1:{s}:{s}"),
        // "3000:8080": host:container. Anything longer already names an address.
        2 => format!("127.0.0.1:{s}"),
        _ => s.to_string(),
    }
}

// Builds the `docker run` arguments for a ContainerBackend: the workspace mount
// (or a default bind), the working dir, the settings that devcontainer.json
// states directly (its `mounts` among them), and finally the user's `runArgs`
// (last, so they can override).
fn build_run_args(
    user_args: Option<Vec<String>>,
    workspace_mount: Option<String>,
    settings: &CreateSettings,
    vars: &Vars,
    project_path: &Path,
    workspace_folder: &str,
) -> Vec<String> {
    let mut args = vec![
        "--label".to_string(),
        format!("{LABEL_PROJECT}={}", project_path.display()),
    ];
    match workspace_mount {
        Some(m) => {
            args.push("--mount".into());
            args.push(vars.apply(&m));
        }
        None => {
            args.push("-v".into());
            args.push(format!("{}:{}", project_path.display(), workspace_folder));
        }
    }
    args.push("-w".into());
    args.push(workspace_folder.to_string());
    args.extend(settings.to_args());
    args.extend(
        user_args
            .unwrap_or_default()
            .into_iter()
            .map(|a| vars.apply(&a)),
    );
    args
}

// ── construction from devcontainer.json ────────────────────────────────────────

/// What backend construction needs to know about the project.
pub(super) struct BackendContext<'a> {
    pub(super) project_path: &'a Path,
    pub(super) base_dir: &'a Path,
    pub(super) workspace_folder: &'a str,
    pub(super) vars: &'a Vars<'a>,
}

pub(super) struct BuiltBackend {
    pub(super) backend: Box<dyn Backend>,
    /// Extra files (besides the `.devcontainer` dir) to fold into the drift hash.
    pub(super) hash_extra: Vec<PathBuf>,
    pub(super) is_compose: bool,
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
        }),
        hash_extra,
        is_compose: true,
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
    let run_args = build_run_args(
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
            run_args,
            override_command: create.override_command.unwrap_or(true),
        }),
        hash_extra,
        is_compose: false,
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
        target: build.target,
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
    use serde_json::json;

    fn as_is(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn mounts_parse_from_string_and_object() {
        let m = Mount::parse(&json!("type=bind,source=/a,target=/b,readonly"), &as_is).unwrap();
        assert_eq!(m.spec, "type=bind,source=/a,target=/b,readonly");
        assert_eq!(m.compose_volume().as_deref(), Some("/a:/b"));
        let m = Mount::parse(
            &json!({"type": "volume", "source": "vol", "target": "/data"}),
            &as_is,
        )
        .unwrap();
        assert_eq!(m.spec, "type=volume,source=vol,target=/data");
        // Without a target there is no compose volume to write.
        let m = Mount::parse(&json!("source=/a"), &as_is).unwrap();
        assert_eq!(m.compose_volume(), None);
        assert!(Mount::parse(&json!(3), &as_is).is_none());
    }

    #[test]
    fn mounts_resolve_variables_including_the_container_workspace() {
        let p = Path::new("/home/me/proj");
        let vars = Vars::new(p).with_container_workspace("/workspaces/proj");
        let obj = json!({
            "type": "bind",
            "source": "${localWorkspaceFolder}/cache",
            "target": "${containerWorkspaceFolder}/.cache"
        });
        assert_eq!(
            Mount::parse(&obj, &|s| vars.apply(s)).unwrap().spec,
            "type=bind,source=/home/me/proj/cache,target=/workspaces/proj/.cache"
        );
    }

    #[test]
    fn settings_become_run_flags() {
        let s = CreateSettings {
            privileged: true,
            init: true,
            cap_add: vec!["SYS_PTRACE".into()],
            security_opt: vec!["seccomp=unconfined".into()],
            mounts: vec![Mount::parse(&json!("type=volume,source=v,target=/d"), &as_is).unwrap()],
            container_user: Some("dev".into()),
            published_ports: vec!["127.0.0.1:3000:3000".into()],
        };
        assert_eq!(
            s.to_args(),
            [
                "--privileged",
                "--init",
                "--cap-add",
                "SYS_PTRACE",
                "--security-opt",
                "seccomp=unconfined",
                "--mount",
                "type=volume,source=v,target=/d",
                "-u",
                "dev",
                "-p",
                "127.0.0.1:3000:3000"
            ]
        );
        assert!(CreateSettings::default().to_args().is_empty());
    }

    #[test]
    fn merged_settings_keep_both_sides() {
        let mut a = CreateSettings {
            cap_add: vec!["A".into()],
            container_user: Some("dev".into()),
            ..Default::default()
        };
        a.merge(CreateSettings {
            privileged: true,
            cap_add: vec!["B".into()],
            ..Default::default()
        });
        assert!(a.privileged);
        assert_eq!(a.cap_add, ["A", "B"]);
        assert_eq!(a.container_user.as_deref(), Some("dev"));
    }

    #[test]
    fn the_compose_override_is_quoted_yaml() {
        let s = CreateSettings {
            privileged: true,
            init: true,
            cap_add: vec!["SYS_PTRACE".into()],
            mounts: vec![Mount::parse(&json!("source=v,target=/d"), &as_is).unwrap()],
            ..Default::default()
        };
        let ov = s.compose_override("dev", &[(LABEL_PROJECT, "/p/it's: here")]);
        assert_eq!(
            ov,
            "services:\n  \"dev\":\n    privileged: true\n    init: true\n    \
             cap_add:\n      - \"SYS_PTRACE\"\n    volumes:\n      - \"v:/d\"\n    \
             labels:\n      \"dev.lodge.project\": \"/p/it's: here\"\n"
        );
    }

    #[test]
    fn app_ports_are_published_on_loopback_only() {
        let ports = parse_app_ports(Some(json!([3000, "4000", "8080:80", "0.0.0.0:9:9"])));
        assert_eq!(
            ports,
            [
                "127.0.0.1:3000:3000",
                "127.0.0.1:4000:4000",
                "127.0.0.1:8080:80",
                "0.0.0.0:9:9" // an explicit address is the user's decision
            ]
        );
        assert_eq!(parse_app_ports(Some(json!(5000))), ["127.0.0.1:5000:5000"]);
        assert!(parse_app_ports(None).is_empty());
    }

    #[test]
    fn user_run_args_come_last_so_they_can_override() {
        let p = Path::new("/home/me/proj");
        let args = build_run_args(
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
            args,
            [
                "--label",
                "dev.lodge.project=/home/me/proj",
                "-v",
                "/home/me/proj:/workspaces/proj",
                "-w",
                "/workspaces/proj",
                "--privileged",
                "--cap-drop=ALL"
            ]
        );
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
