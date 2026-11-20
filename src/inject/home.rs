//! The session user's home directory: making sure the user can write to it,
//! and owns what lodge puts under it.

use anyhow::{Context, Result};
use std::path::Path;

use crate::runtime::{ContainerUser, Env, ExecOpts};

/// Run as the user: whether they can write to their `$HOME`.
pub(super) const HOME_WRITABLE_SCRIPT: &str = "test -w \"$HOME\"";

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
    let writable = env
        .output(
            &["sh", "-c", HOME_WRITABLE_SCRIPT],
            &ExecOpts::as_user(user.exec_as()),
        )
        .context("checking that the home directory is writable")?
        .success;
    if !writable {
        // A user whose home doesn't exist at all (`nobody` → /nonexistent)
        // gets one: injected configs and the tools' own state live there.
        if !env.exists(&user.home)? {
            env.mkdir_p(&[&user.home])?;
        }
        env.exec_root(&["chown", &user.owner(), &user.home])?;
    }
    Ok(())
}

// Chowns every path component between home (exclusive) and `dest` (inclusive,
// recursive) so the container user owns the full chain and can create siblings
// (e.g. nvim writing .local/state next to .local/share) at runtime. Nothing to
// do for root, or for a destination outside the home.
pub(super) fn chown_home_chain(env: &dyn Env, user: &ContainerUser, dest: &str) -> Result<()> {
    if user.is_root() || !user.is_in_home(dest) {
        return Ok(());
    }
    let home_path = Path::new(&user.home);
    let dest_path = Path::new(dest);
    let owner = user.owner();

    // The dirs between home and dest (home excluded, dest included).
    let mut chain: Vec<&Path> = dest_path
        .ancestors()
        .filter(|p| p.starts_with(home_path) && *p != home_path)
        .collect();
    chain.reverse();

    for dir in &chain {
        let s = dir.to_string_lossy();
        if *dir == dest_path {
            env.exec_root(&["chown", "-R", &owner, &s])?;
        } else {
            env.exec_root(&["chown", &owner, &s])?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeEnv, Who};

    fn user(home: &str) -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: home.into(),
        }
    }

    const PROBE: [&str; 3] = ["sh", "-c", HOME_WRITABLE_SCRIPT];

    #[test]
    fn root_and_a_writable_home_are_left_alone() {
        let env = FakeEnv::new();
        ensure_home_writable(&env, &ContainerUser::root()).unwrap();
        assert!(env.calls().is_empty(), "root is not even asked");

        let env = FakeEnv::new().with_output(&PROBE, "");
        ensure_home_writable(&env, &user("/home/dev")).unwrap();
        assert_eq!(env.ran_as(&PROBE), Some(Who::Named("dev".into())));
        assert!(!env.ran_matching(|c| c[0] == "chown"));
    }

    #[test]
    fn a_home_the_user_cannot_write_is_given_to_them_and_only_it() {
        let env = FakeEnv::new()
            .with_failure(&PROBE)
            .with_file("/home/dev/.ssh/key", "");
        ensure_home_writable(&env, &user("/home/dev")).unwrap();
        assert_eq!(
            env.ran_as(&["chown", "1000:1000", "/home/dev"]),
            Some(Who::Root)
        );
        assert!(!env.ran(&["mkdir", "-p", "/home/dev"]), "it exists");
    }

    #[test]
    fn a_missing_home_is_made() {
        let env = FakeEnv::new().with_failure(&PROBE);
        ensure_home_writable(&env, &user("/nonexistent")).unwrap();
        assert!(env.ran(&["mkdir", "-p", "/nonexistent"]));
        assert!(env.ran(&["chown", "1000:1000", "/nonexistent"]));
    }

    #[test]
    fn the_chain_from_home_to_a_destination_is_the_users() {
        let env = FakeEnv::new();
        chown_home_chain(&env, &user("/home/dev"), "/home/dev/.local/share/nvim").unwrap();
        let chowns: Vec<Vec<String>> = env
            .calls()
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| c[0] == "chown")
            .collect();
        assert_eq!(
            chowns,
            [
                vec!["chown", "1000:1000", "/home/dev/.local"],
                vec!["chown", "1000:1000", "/home/dev/.local/share"],
                vec!["chown", "-R", "1000:1000", "/home/dev/.local/share/nvim"],
            ]
        );
        let env = FakeEnv::new();
        chown_home_chain(&env, &user("/home/dev"), "/etc/x").unwrap();
        assert!(env.calls().is_empty(), "outside the home");
    }
}
