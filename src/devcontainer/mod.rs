//! Running a project's devcontainer: bring the container up, then run the
//! pieces the spec defines around it.
//!
//! [`start`] is the orchestration; each step lives in its own module:
//!
//! - [`config`]: parse `devcontainer.json` into a [`Spec`](config::Spec)
//! - [`backend`]: create/start the container (compose, or image/build)
//! - [`hash`]: detect that the config drifted since the container was created
//! - [`lifecycle`]: `initializeCommand`, `onCreateCommand`, … and container env
//! - [`uid`]: line the container user's uid up with the host user's
//! - [`vars`]: `${…}` substitution in config values
//! - [`workdir`]: choose a fallback working directory
//! - [`remote`]: the `--remote` equivalents of the file reads above

mod backend;
mod config;
mod hash;
mod lifecycle;
mod remote;
mod uid;
mod vars;
mod workdir;

use crate::output::{info, note, prompt, warning};
use anyhow::Result;
use std::io::{self, IsTerminal};
use std::path::Path;

use crate::forward::PortForward;
use crate::runtime::{Container, Env, Runtime};
use config::load_spec;
use hash::{hash_inputs, read_hash, write_hash};
use lifecycle::{Cmd, exec_env, run_in_container, run_on_host, write_env_profile};
use workdir::select_workdir;

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
    /// environment and right before attaching the interactive session.
    pub fn run_post_attach(&self) -> Result<()> {
        if let Some(cmd) = &self.post_attach {
            run_in_container(
                self.container.as_ref(),
                &self.workdir,
                &self.env,
                "postAttachCommand",
                cmd,
            );
        }
        Ok(())
    }
}

pub fn start(project_path: &Path, force_build: bool, rt: &dyn Runtime) -> Result<Started> {
    let target = rt.target();
    let spec = load_spec(project_path, target)?;
    let current_hash = hash_inputs(&spec.config_path, &spec.hash_extra, target)?;

    // Is there already a container for this devcontainer, and has its config
    // drifted since it was created?
    let existing = spec.backend.existing(rt)?;
    let drifted = !force_build
        && match &existing {
            Some(id) => {
                read_hash(rt.container(id).as_ref()).as_deref() != Some(current_hash.as_str())
            }
            None => false,
        };
    let recreate = drifted
        && confirm(
            ".devcontainer changed since this container was created — recreate it? \
             (anything installed in it since will be lost)",
        )?;
    let created = force_build || existing.is_none() || recreate;

    // initializeCommand runs on the host before the container is created/started.
    if let Some(cmd) = &spec.lifecycle.initialize {
        run_on_host(&spec.base_dir, "initializeCommand", cmd, target)?;
    }

    // When (re)creating, resolve features up front so their container-creation
    // flags (mounts/privileged/…) can be applied at `up`. On a reused container
    // those flags are already baked in, so we skip the fetch and just retry any
    // not-yet-installed features afterwards (marker-gated).
    let create_plan = if created {
        crate::features::resolve(&spec.base_dir, &spec.features, &spec.override_feature_order)?
    } else {
        crate::features::Plan::default()
    };

    let id = spec
        .backend
        .up(rt, force_build || recreate, &create_plan.create_flags())?;
    let container = rt.container(&id);
    let env: &dyn Env = container.as_ref();

    if created {
        // Record the hash so future runs can detect drift.
        write_hash(env, &current_hash)?;
    }

    // Line the container user's uid up with the host user's so the bind-mounted
    // workspace is writable. Only when the container was just created.
    if created && spec.sync_uid {
        match target
            .uid_gid()
            .and_then(|(uid, gid)| uid::sync_user_uid(env, spec.remote_user.as_deref(), uid, gid))
        {
            Ok(()) => {}
            Err(e) => {
                warning!("could not align the container user with the host (continuing): {e:#}")
            }
        }
    }

    // Persist containerEnv/remoteEnv so login shells inside the container see it.
    write_env_profile(env, &spec.env)?;
    let hook_env = exec_env(&spec.env);

    // Features install before the create hooks (the spec bakes them into the
    // image first). On a fresh container we install the resolved plan; on a
    // reused one, install() is marker-gated — a cheap no-op once everything
    // succeeded, retrying anything that previously failed.
    if created {
        create_plan.install(env)?;
    } else {
        crate::features::install(
            env,
            &spec.base_dir,
            &spec.features,
            &spec.override_feature_order,
        )?;
    }

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
            if env.task_done(key) {
                continue;
            }
            if run_in_container(env, &spec.workspace_folder, &hook_env, label, c) {
                env.mark_task_done(key)?;
            }
        }
    }

    // postStartCommand runs on every start; a failure is a warning, not fatal.
    if let Some(c) = &spec.lifecycle.post_start {
        run_in_container(
            env,
            &spec.workspace_folder,
            &hook_env,
            "postStartCommand",
            c,
        );
    }

    let workdir = if env.path_exists(&spec.workspace_folder) {
        spec.workspace_folder
    } else {
        select_workdir(container.as_ref(), &spec.workspace_folder)
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

/// Stops the container `ldg up` manages for this project, if one exists.
/// The container is kept so `ldg up` can resume it with everything intact.
pub fn stop(project_path: &Path, rt: &dyn Runtime) -> Result<()> {
    let spec = load_spec(project_path, rt.target())?;
    match spec.backend.existing(rt)? {
        Some(id) => {
            info!("stopping {id}");
            rt.stop(&id)
        }
        None => {
            info!("no container exists for this devcontainer");
            Ok(())
        }
    }
}

/// Removes the project's container, throwing away everything installed in it.
/// The next `ldg up` creates a fresh one.
pub fn remove(project_path: &Path, rt: &dyn Runtime) -> Result<()> {
    let spec = load_spec(project_path, rt.target())?;
    match spec.backend.existing(rt)? {
        Some(id) => {
            info!("removing {id}");
            spec.backend.remove(rt)
        }
        None => {
            info!("no container exists for this devcontainer");
            Ok(())
        }
    }
}

/// Prints the state of the project's container and whether its config has
/// changed since it was created.
pub fn status(project_path: &Path, rt: &dyn Runtime) -> Result<()> {
    let spec = load_spec(project_path, rt.target())?;
    info!("project    {}", project_path.display());
    let Some(id) = spec.backend.existing(rt)? else {
        info!("container  none (run `ldg up` to create it)");
        return Ok(());
    };
    let container = rt.container(&id);
    let name = container.name().unwrap_or_else(|| id.clone());
    let state = if container.is_running() {
        "running"
    } else {
        "stopped"
    };
    info!("container  {name} ({state})");

    let current = hash_inputs(&spec.config_path, &spec.hash_extra, rt.target())?;
    let config = match read_hash(container.as_ref()) {
        Some(stored) if stored == current => "up to date",
        Some(_) => "changed since the container was created (`ldg up --build` recreates it)",
        None => "unknown (the container has no recorded config)",
    };
    info!("config     {config}");
    if let Some(id) = crate::inject::installed_bundle(container.as_ref()) {
        info!("bundle     {id}");
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        note!(
            "{question}\n(no TTY — keeping the existing container; \
             re-run with `ldg up --build` to recreate)"
        );
        return Ok(false);
    }
    prompt!("[lodge] {question} [y/N] ");
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}
