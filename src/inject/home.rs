//! The session user's home directory: making sure the user can write to it,
//! and owns what lodge puts under it.

use anyhow::{Context, Result};
use std::path::Path;

use crate::runtime::{ContainerUser, Env, ExecOpts};

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
    // Asked as the user. Not being able to ask is an error, not "not
    // writable": the answer decides whether to chown a folder that may be the
    // host's.
    let opts = ExecOpts {
        user: user.exec_as(),
        ..ExecOpts::default()
    };
    let writable = env
        .output(&["sh", "-c", "test -w \"$HOME\""], &opts)
        .context("checking that the home directory is writable")?
        .success;
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
pub(super) fn chown_home_chain(env: &dyn Env, user: &ContainerUser, target: &str) -> Result<()> {
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
