//! Injecting config files and directories.

use anyhow::Result;
use std::path::Path;

use super::home::chown_home_chain;
use super::installed::{Decision, decide, record};
use crate::bundle::manifest::{ConfigTree, Tool};
use crate::bundle::source::Prepared;
use crate::output::info;
use crate::runtime::{ContainerUser, Env, ExecOpts};

pub(super) fn inject_config(
    env: &dyn Env,
    user: &ContainerUser,
    bundle: &Prepared,
    tool: &Tool,
    config: &ConfigTree,
) -> Result<()> {
    let name = &tool.name;
    let target = user.expand_home(&config.target);
    let key = format!("{name}.config");

    let decision = decide(
        env,
        &key,
        &config.hash,
        env.path_exists(&target),
        tool.skip_if_exists,
    );
    if decision != Decision::Install {
        if decision == Decision::UpToDate {
            info!("{name} config is up to date");
        } else {
            info!("{name} config already present, skipping");
        }
        // Fix ownership even when skipping the copy — the files may be
        // root-owned from a previous run.
        chown_home_chain(env, user, &target)?;
        return Ok(());
    }

    info!("injecting {name} config → {target}");

    // Create parent dirs as the container user when the target is under their
    // home directory — `mkdir -p` as root would leave the whole chain
    // (e.g. /home/dev/.local) root-owned, blocking the user from creating
    // siblings later. For system paths (e.g. /usr/local/share) root is correct.
    let parent = Path::new(&target)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    if !user.is_root() && user.owns_path(&target) {
        env.exec(
            &["mkdir", "-p", &parent],
            &ExecOpts {
                user: user.exec_as(),
                ..ExecOpts::default()
            },
        )?;
    } else {
        env.mkdir_p(&[&parent])?;
    }

    // Replace any existing target before copying: `cp_in` merges a directory
    // into an existing one, so removing first makes the copy mirror the bundle
    // exactly (and drops files that are no longer in it).
    env.exec_root(&["rm", "-rf", &target])?;
    env.cp_in(&bundle.config(name).to_string_lossy(), &target)?;

    // Fix ownership of the entire path chain from home to target. docker cp
    // (especially in remote mode) may recreate intermediate dirs as root.
    // Chowning the chain ensures the user can create siblings (e.g.
    // .local/state) at runtime.
    chown_home_chain(env, user, &target)?;

    record(env, &key, &config.hash, &target)
}
