//! Running a project's devcontainer: bring the container up, then run the
//! pieces the spec defines around it.
//!
//! [`Devcontainer`] provisions a project (its `start` is the orchestration); each step lives in its own module:
//!
//! - [`spec`]: parse `devcontainer.json` into a [`Spec`]
//! - [`backend`]: create/start the container (compose, or image/build)
//! - [`create`]: how a container is created (flags, mounts, published ports)
//! - [`features`]: fetch, order and install devcontainer features
//! - [`drift`]: detect that the config drifted since the container was created
//! - [`lifecycle`]: `initializeCommand`, `onCreateCommand`, … and container env
//! - [`markers`]: remember which one-shot hooks and installs succeeded
//! - [`profile`]: the env written for login shells
//! - [`uid`]: line the container user's uid up with the host user's
//! - [`vars`]: `${…}` substitution in config values
//! - [`workdir`]: choose a fallback working directory
//!
//! Anything that needs a person to decide goes through [`Prompt`], which the
//! command layer implements.

mod backend;
mod create;
mod drift;
mod features;
mod lifecycle;
mod markers;
mod profile;
mod spec;
mod uid;
mod vars;
mod workdir;

use crate::output::{note, warning};
use anyhow::Result;
use std::path::Path;

use crate::host::HostRef;
use crate::local::Local;
use crate::provision::{Attached, PostAttach, Provisioner};
use crate::runtime::{Container, Env, Runtime};
use crate::tools::OciFetcher;
use drift::{config_state, write_hash};
use lifecycle::{HookCtx, exec_env, run_create_hooks, run_in_container, run_on_host};
use profile::write_env_profile;
use spec::{Spec, load_spec};

pub use backend::LABEL_PROJECT;
pub use drift::ConfigState;

/// The questions [`start`] may need a person to answer.
pub trait Prompt {
    /// The directory to work in when the configured `workspaceFolder` doesn't
    /// exist in the container, offered `candidates` that do.
    fn choose_workdir(&self, configured: &str, candidates: &[String]) -> String;
}

/// The devcontainer provisioner: brings a project's devcontainer up on `host`
/// (where the engine `rt` runs too). Registry features are pulled with `oci`,
/// and anything that needs a person goes through `prompt`.
pub struct Devcontainer<'a> {
    pub host: &'a HostRef,
    /// The machine lodge runs on, which `${localEnv:…}` reads.
    pub local: &'a dyn Local,
    pub rt: &'a dyn Runtime,
    pub oci: &'a dyn OciFetcher,
    pub prompt: &'a dyn Prompt,
}

impl Provisioner for Devcontainer<'_> {
    fn provision(&self, project: &Path, rebuild: bool) -> Result<Attached> {
        start(
            project,
            self.host,
            self.local,
            rebuild,
            self.rt,
            self.oci,
            self.prompt,
        )
    }
}

/// Brings up the devcontainer of `project_path`, whose files are on `host`
/// (where the engine `rt` runs too); registry features are pulled with `oci`.
pub fn start(
    project_path: &Path,
    host: &HostRef,
    local: &dyn Local,
    build: bool,
    rt: &dyn Runtime,
    oci: &dyn OciFetcher,
    prompt: &dyn Prompt,
) -> Result<Attached> {
    let (spec, existing) = locate(project_path, host, local, rt)?;
    let current_hash = spec.hash(host.as_ref())?;

    // Has the config drifted since the existing container was created? It is
    // only reported: `--build` is what recreates.
    if !build
        && existing.as_deref().is_some_and(|id| {
            config_state(rt.container(id).as_ref(), &current_hash) == ConfigState::Changed
        })
    {
        note!(
            "the devcontainer config changed since this container was created \
             (`ldg up --build` recreates it; anything installed in it is lost)"
        );
    }
    let created = build || existing.is_none();

    // initializeCommand runs on the host before the container is created/started.
    if let Some(cmd) = &spec.lifecycle.initialize {
        run_on_host(&spec.base_dir, "initializeCommand", cmd, host.as_ref())?;
    }

    let features_ctx = features::Context {
        base_dir: &spec.base_dir,
        host: host.as_ref(),
        oci,
    };

    // A new container needs every feature, resolved up front so their
    // container-creation settings (mounts, privileged, …) apply when it is
    // created. A reused one has those baked in; it only retries the features
    // that haven't succeeded yet, resolved once it is up.
    let mut plan = if created {
        features::resolve(&features_ctx, &spec.features, &spec.override_feature_order)?
    } else {
        features::Plan::default()
    };

    let id = spec
        .backend
        .up(rt, existing.as_deref(), build, &plan.create_settings())?;
    let container = rt.container(&id);
    let env: &dyn Env = container.as_ref();

    if created {
        // Record the hash so future runs can detect drift.
        write_hash(env, &current_hash)?;
    }

    // Line the container user's uid up with the host user's so the bind-mounted
    // workspace is writable. Only when the container was just created.
    if created
        && spec.sync_uid
        && let Err(e) = host
            .uid_gid()
            .and_then(|(uid, gid)| uid::sync_user_uid(env, spec.remote_user.as_deref(), uid, gid))
    {
        warning!("could not align the container user with the host (continuing): {e:#}");
    }

    // Persist containerEnv/remoteEnv so login shells inside the container see it.
    write_env_profile(env, &spec.env)?;
    let hook_env = exec_env(&spec.env);

    // Features install before the create hooks (the spec bakes them into the
    // image first).
    if !created {
        let pending = features::pending(env, &spec.features);
        plan = features::resolve(&features_ctx, &pending, &spec.override_feature_order)?;
    }
    plan.install(env, spec.remote_user.as_deref())?;

    let hooks = HookCtx {
        workdir: &spec.workspace_folder,
        user: spec.remote_user.as_deref(),
        vars: &hook_env,
    };
    run_create_hooks(env, &spec.lifecycle, &hooks)?;
    // postStartCommand runs on every start; a failure is a warning, not fatal.
    if let Some(c) = &spec.lifecycle.post_start {
        run_in_container(env, &hooks, "postStartCommand", c);
    }

    let workdir = workdir::resolve(container.as_ref(), &spec.workspace_folder, prompt);
    // Runs `postAttachCommand`, once lodge has injected and right before the
    // session. A failure is reported, never fatal.
    let post_attach = spec.lifecycle.post_attach.map(|cmd| -> PostAttach {
        let workdir = workdir.clone();
        let user = spec.remote_user.clone();
        Box::new(move |env| {
            let ctx = HookCtx {
                workdir: &workdir,
                user: user.as_deref(),
                vars: &hook_env,
            };
            run_in_container(env, &ctx, "postAttachCommand", &cmd);
        })
    });
    Ok(Attached {
        container,
        created,
        workdir,
        forward_ports: spec.forward_ports,
        remote_user: spec.remote_user,
        post_attach,
    })
}

/// The project's spec and the id of its existing container, if there is one.
fn locate(
    project_path: &Path,
    host: &HostRef,
    local: &dyn Local,
    rt: &dyn Runtime,
) -> Result<(Spec, Option<String>)> {
    let spec = load_spec(project_path, host, local)?;
    let existing = spec.backend.existing(rt)?;
    Ok((spec, existing))
}

/// The project's container, if it has one.
pub struct Found {
    pub container: Box<dyn Container>,
    /// `remoteUser`, the user sessions and hooks run as.
    pub remote_user: Option<String>,
}

/// Finds the container `ldg up` manages for this project.
pub fn find(
    project_path: &Path,
    host: &HostRef,
    local: &dyn Local,
    rt: &dyn Runtime,
) -> Result<Option<Found>> {
    let (spec, existing) = locate(project_path, host, local, rt)?;
    Ok(existing.map(|id| Found {
        container: rt.container(&id),
        remote_user: spec.remote_user,
    }))
}

/// Stops the container `ldg up` manages for this project, if one exists, and
/// returns its id. The container is kept so `ldg up` can resume it.
pub fn stop(
    project_path: &Path,
    host: &HostRef,
    local: &dyn Local,
    rt: &dyn Runtime,
) -> Result<Option<String>> {
    let (_, existing) = locate(project_path, host, local, rt)?;
    if let Some(id) = &existing {
        rt.stop(id)?;
    }
    Ok(existing)
}

/// Removes the project's container (and compose project), throwing away
/// everything installed in it, and returns its id.
pub fn remove(
    project_path: &Path,
    host: &HostRef,
    local: &dyn Local,
    rt: &dyn Runtime,
) -> Result<Option<String>> {
    let (spec, existing) = locate(project_path, host, local, rt)?;
    if existing.is_some() {
        spec.backend.remove(rt)?;
    }
    Ok(existing)
}

/// What [`status`] finds out about a project's container.
pub struct Status {
    pub found: Found,
    pub running: bool,
    /// Whether the config drifted (read from the container, running or not).
    pub config: ConfigState,
}

/// The project's container, if it has one: whether it runs, and whether its
/// config drifted.
pub fn status(
    project_path: &Path,
    host: &HostRef,
    local: &dyn Local,
    rt: &dyn Runtime,
) -> Result<Option<Status>> {
    let (spec, existing) = locate(project_path, host, local, rt)?;
    let Some(id) = existing else {
        return Ok(None);
    };
    let container = rt.container(&id);
    let running = container.is_running()?;
    let current = spec.hash(host.as_ref())?;
    let config = config_state(container.as_ref(), &current);
    Ok(Some(Status {
        found: Found {
            container,
            remote_user: spec.remote_user,
        },
        running,
        config,
    }))
}

/// Which optional host tools a project's devcontainer needs.
pub struct Needs {
    /// The config that was read.
    pub config_path: std::path::PathBuf,
    /// It uses `dockerComposeFile`.
    pub compose: bool,
    /// It uses features from a registry (so `oras`).
    pub registry_features: bool,
}

/// What the devcontainer in `project_path` needs. An error when there is no
/// config there, or it can't be read.
pub fn needs(project_path: &Path, host: &HostRef, local: &dyn Local) -> Result<Needs> {
    let spec = load_spec(project_path, host, local)?;
    Ok(Needs {
        compose: spec.backend.uses_compose(),
        registry_features: spec
            .features
            .iter()
            .any(|f| !features::is_local_ref(&f.reference)),
        config_path: spec.config_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostRef;
    use crate::host::fake::FakeHost;
    use crate::local::fake::FakeLocal;
    use crate::runtime::fake::{FakeEnv, FakeRuntime};
    use crate::tools::NoRegistry;
    use std::sync::Arc;

    /// The prompt for a workspace that exists: nothing to choose.
    struct Nothing;

    impl Prompt for Nothing {
        fn choose_workdir(&self, configured: &str, _: &[String]) -> String {
            panic!("{configured} exists, nothing to choose")
        }
    }

    /// A project on a host that has nothing else: its folder and the host the
    /// project's files are on.
    struct Project {
        fake: Arc<FakeHost>,
        host: HostRef,
    }

    impl Project {
        fn path(&self) -> &std::path::Path {
            std::path::Path::new("/proj")
        }
    }

    /// A project whose devcontainer.json is `json`: an image-based
    /// devcontainer needs nothing but the engine.
    fn project(json: &str) -> Project {
        let fake = Arc::new(FakeHost::new());
        let project = Project {
            host: fake.clone(),
            fake,
        };
        write_config(&project, json);
        project
    }

    fn write_config(project: &Project, json: &str) {
        project
            .fake
            .put_file("/proj/.devcontainer/devcontainer.json", json);
    }

    const PLAIN: &str =
        r#"{"image": "debian", "workspaceFolder": "/w", "updateRemoteUserUID": false}"#;
    const EDITED: &str = r#"{"image": "debian", "workspaceFolder": "/w", "updateRemoteUserUID": false, "name": "x"}"#;

    /// A container whose workspace folder exists.
    fn container() -> FakeEnv {
        FakeEnv::new().with_file("/w", "")
    }

    /// The engine, with the project's container already there when `existing`.
    fn engine(dir: &Project, existing: bool) -> FakeRuntime {
        let rt = FakeRuntime::new(container);
        if existing {
            rt.with_container(&backend::container_name(dir.path()), container())
        } else {
            rt
        }
    }

    fn state(dir: &Project, rt: &FakeRuntime) -> ConfigState {
        status(dir.path(), &dir.host, &FakeLocal::new(), rt)
            .unwrap()
            .unwrap()
            .config
    }

    #[test]
    fn a_new_container_is_created_and_records_its_config() {
        let dir = project(PLAIN);
        let rt = engine(&dir, false);
        let started = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert_eq!(started.workdir, "/w");
        assert!(started.created);
        assert!(
            rt.ops().iter().any(|o| o.starts_with("run lodge-")),
            "{:?}",
            rt.ops()
        );
        assert_eq!(state(&dir, &rt), ConfigState::UpToDate);
    }

    #[test]
    fn an_unchanged_container_is_started_not_created() {
        let dir = project(PLAIN);
        let rt = engine(&dir, false);
        start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        let again = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert!(!again.created);
        let ops = rt.ops();
        assert_eq!(
            ops.iter().filter(|o| o.starts_with("run ")).count(),
            1,
            "{ops:?}"
        );
        assert!(ops.last().unwrap().starts_with("start "), "{ops:?}");
    }

    #[test]
    fn a_changed_config_is_reported_and_the_container_kept() {
        let dir = project(PLAIN);
        let rt = engine(&dir, false);
        start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        write_config(&dir, EDITED);
        assert_eq!(state(&dir, &rt), ConfigState::Changed);

        let again = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert!(!again.created);
        assert!(!rt.ops().iter().any(|o| o.starts_with("remove")));
        assert_eq!(state(&dir, &rt), ConfigState::Changed, "kept as it was");
    }

    #[test]
    fn a_container_without_a_recorded_config_is_kept_and_reported_unknown() {
        let dir = project(PLAIN);
        let rt = engine(&dir, true);
        let started = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert!(!started.created, "not knowing is not drift");
        let ops = rt.ops();
        assert!(ops.len() == 1 && ops[0].starts_with("start "), "{ops:?}");
        assert_eq!(state(&dir, &rt), ConfigState::Unknown);
    }

    #[test]
    fn a_stopped_container_whose_config_changed_is_still_noticed() {
        let dir = project(PLAIN);
        let rt = engine(&dir, false);
        let started = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        rt.stop(started.container.id()).unwrap();
        write_config(&dir, EDITED);
        // The fake refuses commands in a stopped container, so the record is
        // read without one.
        let s = status(dir.path(), &dir.host, &FakeLocal::new(), &rt)
            .unwrap()
            .unwrap();
        assert!(!s.running);
        assert_eq!(s.config, ConfigState::Changed);
    }

    #[test]
    fn build_recreates_the_container() {
        let dir = project(PLAIN);
        let rt = engine(&dir, true);
        let started = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            true,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert!(started.created);
        let ops = rt.ops();
        assert!(
            ops[0].starts_with("remove ") && ops[1].starts_with("run "),
            "{ops:?}"
        );
        assert_eq!(state(&dir, &rt), ConfigState::UpToDate);
    }

    #[test]
    fn a_compose_devcontainer_is_brought_up_and_then_reused() {
        let dir =
            project(r#"{"dockerComposeFile": "c.yml", "service": "app", "workspaceFolder": "/w"}"#);
        let rt = FakeRuntime::new(container);
        let first = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert!(
            first.container.id().ends_with("-app-1"),
            "{}",
            first.container.id()
        );
        let again = start(
            dir.path(),
            &dir.host,
            &FakeLocal::new(),
            false,
            &rt,
            &NoRegistry,
            &Nothing,
        )
        .unwrap();
        assert_eq!(again.container.id(), first.container.id());
        assert!(!again.created);
        assert_eq!(state(&dir, &rt), ConfigState::UpToDate);
    }
}
