use anyhow::Result;

use crate::output::{info, plural, warning};
use crate::runtime::{Env, ExecOpts};
use crate::shell::shell_quote;

// Registers git `safe.directory` entries for the user the environment acts as
// (`--global` is that user's ~/.gitconfig). Injected plugin repos and the mounted
// workspace are owned by the host uid, which often isn't the session user's, so
// without this git aborts with "detected dubious ownership" and anything that
// shells out to git (lazy.nvim, gitsigns, …) fails. Idempotent: only adds an
// entry that isn't already present. No-op if the target has no git.
pub(super) fn configure_git_safe(env: &dyn Env, dirs: &[String]) -> Result<()> {
    if dirs.is_empty() {
        return Ok(());
    }
    if !env.has_command("git") {
        warning!("git not found in container; skipping safe.directory setup");
        return Ok(());
    }

    info!(
        "marking {}",
        plural(dirs.len(), "git safe.directory", "git safe.directories")
    );
    for dir in dirs {
        let q = shell_quote(dir);
        let script = format!(
            "git config --global --get-all safe.directory 2>/dev/null | grep -qxF {q} \
             || git config --global --add safe.directory {q}"
        );
        env.exec(&["sh", "-c", &script], &ExecOpts::default().repeatable())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeEnv, Who};
    use crate::runtime::{Acting, ContainerUser};

    #[test]
    fn safe_directories_are_set_for_the_user_acted_as() {
        let env = FakeEnv::new().with_commands(&["git"]);
        let user = ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        };
        configure_git_safe(&Acting::new(&env, &user), &["*".to_string()]).unwrap();
        let script = "git config --global --get-all safe.directory 2>/dev/null | grep -qxF '*' \
                      || git config --global --add safe.directory '*'";
        assert_eq!(
            env.ran_as(&["sh", "-c", script]),
            Some(Who::Named("dev".into()))
        );
    }

    #[test]
    fn without_git_or_directories_nothing_is_run() {
        let env = FakeEnv::new();
        configure_git_safe(&env, &[]).unwrap();
        configure_git_safe(&env, &["*".to_string()]).unwrap();
        assert!(!env.ran_matching(|c| c[0] == "sh" && c.iter().any(|a| a.contains("git config"))));
    }
}
