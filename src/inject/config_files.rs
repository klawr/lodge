//! Injecting config files and directories, and the ownership and home-directory
//! handling that goes with them.

use anyhow::Result;
use std::path::Path;

use super::installed::{Decision, decide, record};
use crate::bundle::manifest::{ConfigTree, Tool};
use crate::bundle::source::Prepared;
use crate::output::{info, warning};
use crate::runtime::{Env, ExecOpts};
pub(super) fn inject_config(
    env: &dyn Env,
    home: &str,
    bundle: &Prepared,
    tool: &Tool,
    config: &ConfigTree,
    uid_gid: Option<&str>,
) -> Result<()> {
    let name = &tool.name;
    let target = resolve_home(&config.target, home);
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
        if let Some(owner) = uid_gid {
            chown_home_chain(env, home, &target, owner)?;
        }
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
    if uid_gid.is_some() && target.starts_with(home) {
        env.exec(&["mkdir", "-p", &parent], &ExecOpts::default())?;
    } else {
        env.mkdir_p(&[&parent])?;
    }

    // Replace any existing target before copying: `docker cp` of a directory
    // onto an existing directory nests it (dest/src) instead of overwriting, so
    // removing first makes the copy mirror the bundle exactly (and drops files
    // that are no longer in it).
    env.exec_root(&["rm", "-rf", &target])?;
    env.cp_in(&bundle.config(name).to_string_lossy(), &target)?;

    // Fix ownership of the entire path chain from home to target. The parent
    // dirs were created as the container user above, but docker cp (especially
    // in remote mode) may recreate intermediate dirs as root. Chowning the
    // chain ensures the user can create siblings (e.g. .local/state) at runtime.
    if let Some(owner) = uid_gid {
        chown_home_chain(env, home, &target, owner)?;
    }

    record(env, &key, &config.hash)
}

// When the container user can't already write to their own `$HOME` (e.g. it
// got auto-created as root — by Docker for a path with no image content, or
// by an earlier lodge run's `mkdir -p`, which runs as the container's default
// user and so is root whenever that differs from `remoteUser`), chowns just
// `home` itself, non-recursively. Gated on the writability probe rather than
// done unconditionally: `home` is a directory lodge didn't create, and may be
// a bind mount from the host (chowning unconditionally would chown host
// files) or — if `owner` were ever misdetected — could lock the real user out
// of a `$HOME` that was already correctly owned. Non-recursive because
// existing children (e.g. a mounted `.ssh`/`.gnupg`) may be intentionally
// owned by someone else.
pub(super) fn ensure_home_writable(
    env: &dyn Env,
    home: &str,
    remote_user: Option<&str>,
    owner: &str,
) -> Result<()> {
    let writable = env
        .exec_capture_as(remote_user, &["sh", "-c", "test -w \"$HOME\""])
        .is_ok();
    if !writable {
        env.exec_root(&["chown", owner, home])?;
    }
    Ok(())
}

// Chowns every path component between home (exclusive) and target (inclusive,
// recursive) so the container user owns the full chain and can create siblings
// (e.g. nvim writing .local/state next to .local/share) at runtime.
fn chown_home_chain(env: &dyn Env, home: &str, target: &str, owner: &str) -> Result<()> {
    let home_path = Path::new(home);
    let target_path = Path::new(target);

    if !target_path.starts_with(home_path) {
        return Ok(());
    }

    // Collect the dirs between home and target (home excluded, target included).
    let mut chain: Vec<&Path> = target_path
        .ancestors()
        .filter(|p| p.starts_with(home_path) && *p != home_path)
        .collect();
    chain.reverse();

    for dir in &chain {
        let s = dir.to_string_lossy();
        if *dir == target_path {
            env.exec_root(&["chown", "-R", owner, &s])?;
        } else {
            env.exec_root(&["chown", owner, &s])?;
        }
    }

    Ok(())
}

// Detects the container user's home directory. Queries as `remote_user` when
// set so $HOME reflects their account. Falls back to /root on failure.
pub(super) fn container_home(env: &dyn Env, remote_user: Option<&str>) -> String {
    match env.exec_capture_as(remote_user, &["sh", "-c", "echo $HOME"]) {
        Ok(out) if !out.trim().is_empty() => out.trim().to_string(),
        Ok(_) => "/root".to_string(),
        Err(e) => {
            warning!(
                "could not detect the container user's home directory ({e:#}); assuming /root"
            );
            "/root".to_string()
        }
    }
}

// Returns the owner to use for chown, or None when the container runs as root
// and no `remoteUser` override is set (chowning is then unnecessary).
// When `remote_user` is specified, returns it directly as the owner name
// (chown accepts usernames). Otherwise detects uid:gid via `id`.
pub(super) fn container_user(env: &dyn Env, remote_user: Option<&str>) -> Option<String> {
    if let Some(u) = remote_user {
        return if u == "root" {
            None
        } else {
            Some(u.to_string())
        };
    }
    let out = match env.exec_capture_as(
        None,
        &["sh", "-c", "printf '%s:%s' \"$(id -u)\" \"$(id -g)\""],
    ) {
        Ok(out) => out,
        Err(e) => {
            warning!(
                "could not detect the container user ({e:#}); not changing ownership of injected files"
            );
            return None;
        }
    };
    let s = out.trim().to_string();
    if s.starts_with("0:") { None } else { Some(s) }
}

// Expands a leading `~` to the container user's home directory.
pub(super) fn resolve_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_home_expands_tilde() {
        assert_eq!(
            resolve_home("~/.config/nvim", "/home/vscode"),
            "/home/vscode/.config/nvim"
        );
        assert_eq!(resolve_home("~", "/home/vscode"), "/home/vscode");
        assert_eq!(
            resolve_home("/usr/local/bin/nvim", "/home/vscode"),
            "/usr/local/bin/nvim"
        );
    }

    #[test]
    fn resolve_home_strips_trailing_slash_from_home() {
        assert_eq!(resolve_home("~/.config", "/root/"), "/root/.config");
    }
}
