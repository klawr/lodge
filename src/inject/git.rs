use anyhow::{Result, bail};

use crate::output::{info, plural, warning};
use crate::runtime::{Env, ExecOpts};
use crate::shell::shell_quote;

// Registers git `safe.directory` entries for the user the environment acts as
// (`--global` is that user's ~/.gitconfig). Injected plugin repos and the mounted
// workspace are owned by the host uid, which often isn't the session user's, so
// without this git aborts with "detected dubious ownership" and anything that
// shells out to git (lazy.nvim, gitsigns, …) fails. Idempotent: only adds an
// entry that isn't already present. No-op if the target has no git.
//
// Returns the entries lodge is responsible for, which `ldg clean` removes: those
// in `before` (an earlier injection added them, so they are present now and
// would look like the user's) and those added now. An entry that was already
// there, or that git already had, is the user's and never listed.
pub(super) fn configure_git_safe(
    env: &dyn Env,
    dirs: &[String],
    before: &[String],
) -> Result<Vec<String>> {
    let mut ours = before.to_vec();
    // `*` switches git's ownership check off for every folder, which is for a
    // throwaway container, not for someone's real machine.
    let wildcard_refused = !env.is_disposable() && dirs.iter().any(|d| d == "*");
    let dirs: Vec<String> = if wildcard_refused {
        dirs.iter().filter(|d| *d != "*").cloned().collect()
    } else {
        dirs.to_vec()
    };
    let dirs = dirs.as_slice();
    if wildcard_refused {
        warning!(
            "not setting git safe.directory = * on a machine that is not thrown away; list the folders in [git] safe_directories"
        );
    }
    if dirs.is_empty() {
        return Ok(ours);
    }
    if !env.has_command("git") {
        warning!("git not found in container; skipping safe.directory setup");
        return Ok(ours);
    }

    info!(
        "marking {}",
        plural(dirs.len(), "git safe.directory", "git safe.directories")
    );
    for dir in dirs {
        let q = shell_quote(dir);
        // Prints "added" only when it adds the entry.
        let script = format!(
            "git config --global --get-all safe.directory 2>/dev/null | grep -qxF {q} \
             || {{ git config --global --add safe.directory {q} && echo added; }}"
        );
        let out = env.output(&["sh", "-c", &script], &ExecOpts::default().repeatable())?;
        if !out.success {
            bail!(
                "setting git safe.directory {dir} failed: {}",
                out.stderr.trim()
            );
        }
        if out.stdout.trim() == "added" && !ours.contains(dir) {
            ours.push(dir.clone());
        }
    }
    Ok(ours)
}

// The script `ldg clean` runs for the entries in `$@`: unsets each (an exact
// match, not a pattern). Git exits 5 when there was no such entry, which is
// already what we want. No git, nothing to unset.
pub(super) const UNSET_SCRIPT: &str = "command -v git >/dev/null 2>&1 || exit 0; status=0; \
     for d in \"$@\"; do \
       git config --global --fixed-value --unset-all safe.directory \"$d\"; \
       rc=$?; [ $rc -eq 0 ] || [ $rc -eq 5 ] || status=1; \
     done; exit $status";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeEnv, Who};
    use crate::runtime::{Acting, ContainerUser};

    fn script(dir: &str) -> String {
        format!(
            "git config --global --get-all safe.directory 2>/dev/null | grep -qxF {dir} \
             || {{ git config --global --add safe.directory {dir} && echo added; }}"
        )
    }

    #[test]
    fn safe_directories_are_set_for_the_user_acted_as() {
        let env = FakeEnv::new()
            .with_commands(&["git"])
            .with_output(&["sh", "-c", &script("'*'")], "added\n");
        let user = ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        };
        configure_git_safe(&Acting::new(&env, &user), &["*".to_string()], &[]).unwrap();
        assert_eq!(
            env.ran_as(&["sh", "-c", &script("'*'")]),
            Some(Who::Named("dev".into()))
        );
    }

    #[test]
    fn only_entries_lodge_added_are_listed_for_clean() {
        let env = FakeEnv::new()
            .with_commands(&["git"])
            .with_output(&["sh", "-c", &script("'/new'")], "added\n")
            .with_output(&["sh", "-c", &script("'/theirs'")], "");
        let dirs = ["/new".to_string(), "/theirs".to_string()];
        let ours = configure_git_safe(&env, &dirs, &[]).unwrap();
        assert_eq!(ours, ["/new"]);
        // A second injection finds its own entry present and still lists it.
        let again = configure_git_safe(&env, &dirs, &ours).unwrap();
        assert_eq!(again, ["/new"]);
    }

    #[test]
    fn without_git_or_directories_nothing_is_run() {
        let env = FakeEnv::new();
        let before = ["/old".to_string()];
        assert_eq!(configure_git_safe(&env, &[], &before).unwrap(), before);
        assert_eq!(
            configure_git_safe(&env, &["*".to_string()], &before).unwrap(),
            before
        );
        assert!(!env.ran_matching(|c| c[0] == "sh" && c.iter().any(|a| a.contains("git config"))));
    }

    #[test]
    fn the_unset_script_treats_a_missing_entry_as_done() {
        use std::process::Command;
        let home = crate::tmpfile::TempDir::new("git-unset").unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .env("HOME", home.path())
                .env("GIT_CONFIG_GLOBAL", home.path().join("gitconfig"))
                .output()
        };
        let Ok(added) = run(&["config", "--global", "--add", "safe.directory", "/a.b"]) else {
            return; // no git here
        };
        assert!(added.status.success());
        run(&["config", "--global", "--add", "safe.directory", "/aXb"]).unwrap();
        let out = Command::new("sh")
            .args(["-c", UNSET_SCRIPT, "sh", "/a.b", "/never-set"])
            .env("HOME", home.path())
            .env("GIT_CONFIG_GLOBAL", home.path().join("gitconfig"))
            .output()
            .unwrap();
        assert!(out.status.success());
        let left = run(&["config", "--global", "--get-all", "safe.directory"]).unwrap();
        assert_eq!(String::from_utf8_lossy(&left.stdout).trim(), "/aXb");
    }

    #[test]
    fn a_real_machine_never_gets_the_wildcard_but_may_name_folders() {
        let env = FakeEnv::new()
            .shared()
            .with_commands(&["git"])
            .with_output(&["sh", "-c", &script("'/w'")], "added\n");
        let ours = configure_git_safe(&env, &["*".to_string(), "/w".to_string()], &[]).unwrap();
        assert_eq!(ours, ["/w"]);
        assert!(!env.ran_matching(|c| c.iter().any(|a| a.contains("'*'"))));
        let only_wildcard = configure_git_safe(&env, &["*".to_string()], &[]).unwrap();
        assert!(only_wildcard.is_empty());
    }
}
