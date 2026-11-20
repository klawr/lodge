//! Running a project's devcontainer: bring the container up, then run the
//! pieces the spec defines around it.
//!
//! [`start`] is the orchestration; each step lives in its own module:
//!
//! - [`config`]: parse `devcontainer.json` into a [`Spec`](config::Spec)
//! - [`backend`]: create/start the container (compose, or image/build)
//! - [`features`]: fetch, order and install devcontainer features
//! - [`hash`]: detect that the config drifted since the container was created
//! - [`lifecycle`]: `initializeCommand`, `onCreateCommand`, … and container env
//! - [`markers`]: remember which one-shot hooks and installs succeeded
//! - [`uid`]: line the container user's uid up with the host user's
//! - [`vars`]: `${…}` substitution in config values
//! - [`workdir`]: choose a fallback working directory
//! - [`remote`]: the `--remote` version of finding the config
//!
//! Anything that needs a person to decide goes through [`Prompt`], which the
//! command layer implements.

mod backend;
mod config;
mod features;
mod hash;
mod lifecycle;
mod markers;
mod remote;
mod uid;
mod vars;
mod workdir;

use crate::output::{note, warning};
use anyhow::Result;
use std::path::Path;

use crate::forward::PortForward;
use crate::runtime::{Container, Env, Runtime};
use crate::target::Target;
use config::{Spec, load_spec};
use hash::{hash_inputs, read_hash, write_hash};
use lifecycle::{Cmd, HookCtx, exec_env, run_in_container, run_on_host, write_env_profile};
use workdir::select_workdir;

pub use backend::LABEL_PROJECT;

/// What to do when the config changed since the container was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recreate {
    /// Ask through the [`Prompt`].
    Ask,
    /// Recreate without asking (`--yes`).
    Yes,
    /// Keep the container without asking (`--no-recreate`).
    No,
}

/// How `ldg up` should treat an existing container.
#[derive(Debug, Clone, Copy)]
pub struct StartOpts {
    /// Rebuild images and recreate the container unconditionally (`--build`).
    pub build: bool,
    pub recreate: Recreate,
}

/// The questions [`start`] may need a person to answer.
pub trait Prompt {
    /// Whether to recreate a container whose config changed (`Recreate::Ask`).
    fn confirm_recreate(&self, question: &str) -> bool;

    /// The directory to work in when the configured `workspaceFolder` doesn't
    /// exist in the container, offered `candidates` that do.
    fn choose_workdir(&self, configured: &str, candidates: &[String]) -> String;
}

/// The result of bringing a devcontainer up: the resolved container, the
/// workspace folder to drop the user into, and the (deferred) postAttach hook.
pub struct Started {
    pub container: Box<dyn Container>,
    pub session_name: String,
    pub workdir: String,
    pub forward_ports: Vec<PortForward>,
    /// The user to run as inside the container (`remoteUser` in devcontainer.json).
    /// `None` means use the container's default CMD user.
    pub remote_user: Option<String>,
    env: Vec<(String, String)>,
    post_attach: Option<Cmd>,
}

impl Started {
    /// Runs `postAttachCommand`, if any. Called after lodge has injected the
    /// environment and right before attaching the interactive session. A
    /// failure is reported, never fatal.
    pub fn run_post_attach(&self) {
        if let Some(cmd) = &self.post_attach {
            let ctx = HookCtx {
                workdir: &self.workdir,
                user: self.remote_user.as_deref(),
                vars: &self.env,
            };
            run_in_container(self.container.as_ref(), &ctx, "postAttachCommand", cmd);
        }
    }
}

pub fn start(
    project_path: &Path,
    opts: StartOpts,
    rt: &dyn Runtime,
    prompt: &dyn Prompt,
) -> Result<Started> {
    let target = rt.target();
    let (spec, existing) = locate(project_path, rt)?;
    let current_hash = hash_inputs(&spec.config_path, &spec.hash_extra, target)?;

    // Has the config drifted since the existing container was created?
    let drifted = !opts.build
        && match &existing {
            Some(id) => match read_hash(rt.container(id).as_ref()) {
                Ok(stored) => stored.as_deref() != Some(current_hash.as_str()),
                // Not knowing is no reason to throw the container away.
                Err(e) => {
                    warning!(
                        "could not check whether the config changed (keeping the container): {e:#}"
                    );
                    false
                }
            },
            None => false,
        };
    let recreate = drifted && should_recreate(opts.recreate, prompt);
    let created = opts.build || existing.is_none() || recreate;

    // initializeCommand runs on the host before the container is created/started.
    if let Some(cmd) = &spec.lifecycle.initialize {
        run_on_host(&spec.base_dir, "initializeCommand", cmd, target)?;
    }

    let features_ctx = features::Context {
        base_dir: &spec.base_dir,
        target,
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
        .up(rt, opts.build || recreate, &plan.create_settings())?;
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
        && let Err(e) = target
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

    // Create-time hooks run once *successfully*: each is skipped if its success
    // marker exists, and the marker is written only when it succeeds — so a
    // failed hook retries on the next `ldg up` rather than only on a recreate.
    // A failure is reported but never aborts lodge.
    for (label, key, cmd) in [
        ("onCreateCommand", "onCreate", &spec.lifecycle.on_create),
        (
            "updateContentCommand",
            "updateContent",
            &spec.lifecycle.update_content,
        ),
        (
            "postCreateCommand",
            "postCreate",
            &spec.lifecycle.post_create,
        ),
    ] {
        if let Some(c) = cmd {
            if markers::done(env, key) {
                continue;
            }
            if run_in_container(env, &hooks, label, c) {
                markers::mark_done(env, key)?;
            }
        }
    }

    // postStartCommand runs on every start; a failure is a warning, not fatal.
    if let Some(c) = &spec.lifecycle.post_start {
        run_in_container(env, &hooks, "postStartCommand", c);
    }

    let workdir = if env.path_exists(&spec.workspace_folder) {
        spec.workspace_folder
    } else {
        select_workdir(container.as_ref(), &spec.workspace_folder, prompt)
    };

    Ok(Started {
        container,
        session_name: spec.session_name,
        workdir,
        forward_ports: spec.forward_ports,
        remote_user: spec.remote_user,
        env: hook_env,
        post_attach: spec.lifecycle.post_attach,
    })
}

/// The project's spec and the id of its existing container, if there is one.
fn locate(project_path: &Path, rt: &dyn Runtime) -> Result<(Spec, Option<String>)> {
    let spec = load_spec(project_path, rt.target())?;
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
pub fn find(project_path: &Path, rt: &dyn Runtime) -> Result<Option<Found>> {
    let (spec, existing) = locate(project_path, rt)?;
    Ok(existing.map(|id| Found {
        container: rt.container(&id),
        remote_user: spec.remote_user,
    }))
}

/// Stops the container `ldg up` manages for this project, if one exists, and
/// returns its id. The container is kept so `ldg up` can resume it.
pub fn stop(project_path: &Path, rt: &dyn Runtime) -> Result<Option<String>> {
    let (_, existing) = locate(project_path, rt)?;
    if let Some(id) = &existing {
        rt.stop(id)?;
    }
    Ok(existing)
}

/// Removes the project's container (and compose project), throwing away
/// everything installed in it, and returns its id.
pub fn remove(project_path: &Path, rt: &dyn Runtime) -> Result<Option<String>> {
    let (spec, existing) = locate(project_path, rt)?;
    if existing.is_some() {
        spec.backend.remove(rt)?;
    }
    Ok(existing)
}

/// Whether the config changed since the container was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigState {
    UpToDate,
    Changed,
    /// The container has no recorded config hash, or it could not be read.
    Unknown,
}

/// The project's container, if it has one, and whether its config drifted.
pub fn status(project_path: &Path, rt: &dyn Runtime) -> Result<Option<(Found, ConfigState)>> {
    let (spec, existing) = locate(project_path, rt)?;
    let Some(id) = existing else {
        return Ok(None);
    };
    let container = rt.container(&id);
    let current = hash_inputs(&spec.config_path, &spec.hash_extra, rt.target())?;
    let state = match read_hash(container.as_ref()) {
        Ok(Some(stored)) if stored == current => ConfigState::UpToDate,
        Ok(Some(_)) => ConfigState::Changed,
        Ok(None) => ConfigState::Unknown,
        Err(e) => {
            warning!("could not read the recorded config: {e:#}");
            ConfigState::Unknown
        }
    };
    let found = Found {
        container,
        remote_user: spec.remote_user,
    };
    Ok(Some((found, state)))
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
pub fn needs(project_path: &Path, target: &Target) -> Result<Needs> {
    let spec = load_spec(project_path, target)?;
    Ok(Needs {
        compose: spec.is_compose,
        registry_features: spec
            .features
            .iter()
            .any(|f| !features::is_local_ref(&f.reference)),
        config_path: spec.config_path,
    })
}

fn should_recreate(policy: Recreate, prompt: &dyn Prompt) -> bool {
    let question = ".devcontainer changed since this container was created — recreate it? \
                    (anything installed in it since will be lost)";
    match policy {
        Recreate::Yes => true,
        Recreate::No => {
            note!("{question}\n(keeping it: --no-recreate)");
            false
        }
        Recreate::Ask => prompt.confirm_recreate(question),
    }
}
