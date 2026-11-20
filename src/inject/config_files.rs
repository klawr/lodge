//! Injecting config files and directories, and the ownership and home-directory
//! handling that goes with them.

use anyhow::Result;
use std::path::Path;

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

    // Replace any existing target before copying: `docker cp` of a directory
    // onto an existing directory nests it (dest/src) instead of overwriting, so
    // removing first makes the copy mirror the bundle exactly (and drops files
    // that are no longer in it).
    env.exec_root(&["rm", "-rf", &target])?;
    env.cp_in(&bundle.config(name).to_string_lossy(), &target)?;

    // Fix ownership of the entire path chain from home to target. docker cp
    // (especially in remote mode) may recreate intermediate dirs as root.
    // Chowning the chain ensures the user can create siblings (e.g.
    // .local/state) at runtime.
    chown_home_chain(env, user, &target)?;

    record(env, &key, &config.hash, &target)
}

// When the container user can't already write to their own `$HOME` (e.g. it
// got auto-created as root — by Docker for a path with no image content, or
// by an earlier lodge run's `mkdir -p` as the container's default user),
// chowns just the home itself, non-recursively. Gated on the writability probe
// rather than done unconditionally: the home is a directory lodge didn't
// create, and may be a bind mount from the host (chowning unconditionally would
// chown host files). Non-recursive because existing children (e.g. a mounted
// `.ssh`/`.gnupg`) may be intentionally owned by someone else.
pub(super) fn ensure_home_writable(env: &dyn Env, user: &ContainerUser) -> Result<()> {
    if user.is_root() {
        return Ok(());
    }
    let writable = env
        .exec_capture_as(user.exec_as(), &["sh", "-c", "test -w \"$HOME\""])
        .is_ok();
    if !writable {
        // A user whose home doesn't exist at all (`nobody` → /nonexistent)
        // gets one: injected configs and the tools' own state live there.
        if !env.path_exists(&user.home) {
            env.mkdir_p(&[&user.home])?;
        }
        env.exec_root(&["chown", &user.owner(), &user.home])?;
    }
    Ok(())
}

// Chowns every path component between home (exclusive) and target (inclusive,
// recursive) so the container user owns the full chain and can create siblings
// (e.g. nvim writing .local/state next to .local/share) at runtime. Nothing to
// do for root, or for a target outside the home.
fn chown_home_chain(env: &dyn Env, user: &ContainerUser, target: &str) -> Result<()> {
    if user.is_root() || !user.owns_path(target) {
        return Ok(());
    }
    let home_path = Path::new(&user.home);
    let target_path = Path::new(target);
    let owner = user.owner();

    // The dirs between home and target (home excluded, target included).
    let mut chain: Vec<&Path> = target_path
        .ancestors()
        .filter(|p| p.starts_with(home_path) && *p != home_path)
        .collect();
    chain.reverse();

    for dir in &chain {
        let s = dir.to_string_lossy();
        if *dir == target_path {
            env.exec_root(&["chown", "-R", &owner, &s])?;
        } else {
            env.exec_root(&["chown", &owner, &s])?;
        }
    }

    Ok(())
}
