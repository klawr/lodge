//! Taking an injection back out of a container (`ldg clean`).
//!
//! Removes what injection put there: the binaries, libraries and linker, the
//! wrappers (only ones that still point at lodge) and the shell setup. Placed
//! secrets are [`crate::secret`]'s to remove. Injected *configs* are left in
//! place and reported, since the user may have changed them in the container.
//! The devcontainer's own state (hook markers, the config hash, `containerEnv`)
//! is kept, so a later `ldg up` neither re-runs create hooks nor thinks the
//! config drifted.

use anyhow::{Result, bail};

use super::installed::{self, Kind};
use crate::layout;
use crate::runtime::{Env, ExecOpts};
use crate::shell::{script_argv, shell_quote};

/// What [`remove`] did.
#[derive(Debug, Default)]
pub struct Removed {
    /// Wrapper scripts that were deleted.
    pub wrappers: Vec<String>,
    /// Injected config files and directories, left in place.
    pub configs_kept: Vec<String>,
}

/// The script: `$@` are recorded wrapper paths. Removes the lodge wrappers
/// among them and among the small files directly in `search` (each removed path
/// is printed), then everything in `owned`. Exits non-zero when anything could
/// not be removed, after trying the rest.
fn script(search: &[&str], owned: &[&str]) -> String {
    // The search folders may hold globs (`/home/*/.local/bin`), so they are left
    // for the shell to expand; they are lodge's own constants.
    let search = search.join(" ");
    let owned: Vec<String> = owned.iter().map(|p| shell_quote(p)).collect();
    format!(
        "status=0; marker={marker}; \
         for p in \"$@\" $(find {search} -maxdepth 1 -type f -size -2k 2>/dev/null); do \
           [ -f \"$p\" ] && grep -q \"$marker\" \"$p\" 2>/dev/null || continue; \
           if rm -f \"$p\"; then echo \"$p\"; else status=1; fi; \
         done; \
         rm -rf {owned} || status=1; \
         exit $status",
        marker = shell_quote(&format!("exec {}/", layout::BIN)),
        owned = owned.join(" "),
    )
}

/// Removes the injection from `env`. An error when something could not be
/// removed (what could be is gone all the same).
pub fn remove(env: &dyn Env) -> Result<Removed> {
    let mut wrappers = Vec::new();
    let mut configs_kept = Vec::new();
    for (kind, path) in installed::placed(env)? {
        match kind {
            Kind::Binary => wrappers.push(path),
            Kind::Config => configs_kept.push(path),
        }
    }
    let script = script(layout::WRAPPER_SEARCH, layout::INJECTED);
    let recorded: Vec<&str> = wrappers.iter().map(String::as_str).collect();
    let out = env.output(&script_argv(&script, &recorded), &ExecOpts::root())?;
    if !out.success {
        bail!("removing lodge's files failed: {}", out.stderr.trim());
    }
    let mut removed: Vec<String> = out.stdout.lines().map(str::to_string).collect();
    removed.sort();
    removed.dedup();
    Ok(Removed {
        wrappers: removed,
        configs_kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn the_script_names_what_injection_owns_and_not_the_devcontainers_state() {
        let s = script(layout::WRAPPER_SEARCH, layout::INJECTED);
        assert!(s.contains("'/opt/lodge/bin'"));
        assert!(s.contains("'/lodge'"));
        assert!(s.contains("find /usr/local/bin"));
        // Hook markers, the config hash and containerEnv stay.
        assert!(!s.contains("/opt/lodge/state"));
        assert!(!s.contains("devcontainer.hash"));
        assert!(!s.contains("lodge-env.sh"));
        assert!(!s.contains("rm -rf '/opt/lodge' "));
    }

    /// Runs the script for real: `search` and `owned` in place of lodge's
    /// folders, `recorded` as the recorded wrappers.
    fn run(search: &Path, owned: &[&Path], recorded: &[&Path]) -> (bool, String) {
        let search = shell_quote(&search.to_string_lossy());
        let owned: Vec<String> = owned.iter().map(|p| p.to_string_lossy().into()).collect();
        let owned: Vec<&str> = owned.iter().map(String::as_str).collect();
        let script = script(&[&search], &owned);
        let recorded: Vec<String> = recorded
            .iter()
            .map(|p| p.to_string_lossy().into())
            .collect();
        let recorded: Vec<&str> = recorded.iter().map(String::as_str).collect();
        let argv = script_argv(&script, &recorded);
        let out = Command::new(argv[0])
            .args(&argv[1..])
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn only_lodge_wrappers_are_removed_with_what_injection_owns() {
        let dir = crate::tmpfile::TempDir::new("wrappers").unwrap();
        let searched = dir.path().join("bin");
        let custom = dir.path().join("custom");
        let owned = dir.path().join("opt-lodge");
        for d in [&searched, &custom, &owned] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(owned.join("lib"), "x").unwrap();
        let ours = searched.join("nvim");
        let theirs = searched.join("other");
        let recorded = custom.join("x");
        std::fs::write(&ours, "#!/bin/sh\nexec /opt/lodge/bin/nvim \"$@\"\n").unwrap();
        std::fs::write(&theirs, "#!/bin/sh\nexec /usr/bin/other \"$@\"\n").unwrap();
        std::fs::write(&recorded, "#!/bin/sh\nexec /opt/lodge/bin/x \"$@\"\n").unwrap();

        // `bin/` is searched; `custom/x`, outside it, is a recorded path.
        let (ok, printed) = run(&searched, &[&owned], &[&recorded]);
        assert!(ok, "{printed}");
        assert!(!ours.exists() && !recorded.exists(), "{printed}");
        assert!(!owned.exists(), "what injection owns is removed");
        assert!(theirs.exists(), "a wrapper that isn't lodge's was removed");
        assert!(
            printed.contains("nvim") && !printed.contains("other"),
            "{printed}"
        );
    }

    #[test]
    fn a_failed_removal_fails_the_script_after_trying_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::tmpfile::TempDir::new("clean-fail").unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(locked.join("inner")).unwrap();
        std::fs::write(locked.join("inner/f"), "x").unwrap();
        let other = dir.path().join("other");
        std::fs::write(&other, "x").unwrap();
        let set = |mode| std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(mode));
        set(0o500).unwrap();
        let (ok, _) = run(dir.path(), &[&locked, &other], &[]);
        set(0o700).unwrap();
        if locked.join("inner").exists() {
            // Not root: the read-only folder kept its contents.
            assert!(!ok, "a removal that failed was reported as success");
        }
        assert!(!other.exists(), "the rest is removed all the same");
    }
}
