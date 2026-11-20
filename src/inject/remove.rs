//! Taking an injection back out of a container (`ldg clean`).
//!
//! Removes what injection put there: the binaries, libraries and linker, the
//! wrappers (only ones that still point at lodge) and the shell setup. Placed
//! secrets are [`crate::secret`]'s to remove. Injected *configs* are left in
//! place and reported, since the
//! user may have changed them in the container. The devcontainer's own state
//! (hook markers, the config hash, `containerEnv`) is kept, so a later `ldg up`
//! neither re-runs create hooks nor thinks the config drifted.

use anyhow::{Result, bail};

use super::installed;
use crate::layout;
use crate::runtime::{Env, ExecOpts};

/// What [`remove`] did.
#[derive(Debug, Default)]
pub struct Removed {
    /// Wrapper scripts that were deleted.
    pub wrappers: Vec<String>,
    /// Injected config files and directories, left in place.
    pub configs_kept: Vec<String>,
}

/// Where wrappers usually live, searched as well: a container injected by an
/// older ldg has no recorded paths, and a wrapper may sit at a custom path.
const WRAPPER_DIRS: &str =
    "/usr/local/bin /usr/local/sbin /usr/bin /bin /root/.local/bin /home/*/.local/bin";

/// The `sh -c` script: `$@` are recorded wrapper paths. Removes the wrappers,
/// then everything injection owns outright.
fn script() -> String {
    let owned: Vec<String> = layout::INJECTED
        .iter()
        .map(|p| crate::shell::shell_quote(p))
        .collect();
    format!(
        "{wrappers} rm -rf {owned}; true",
        wrappers = remove_wrappers(WRAPPER_DIRS),
        owned = owned.join(" "),
    )
}

/// The part of [`script`] that removes wrappers: among `$@` and the small files
/// directly in `dirs`, a file is removed only if it is a lodge wrapper (it
/// `exec`s into the injected binary); each removed path is printed.
fn remove_wrappers(dirs: &str) -> String {
    format!(
        "marker={marker}; \
         for p in \"$@\" $(find {dirs} -maxdepth 1 -type f -size -2k 2>/dev/null); do \
           [ -f \"$p\" ] && grep -q \"$marker\" \"$p\" 2>/dev/null && rm -f \"$p\" && echo \"$p\"; \
         done;",
        marker = crate::shell::shell_quote(&format!("exec {}/", layout::BIN)),
    )
}

/// Removes the injection from `env`.
pub fn remove(env: &dyn Env) -> Result<Removed> {
    let mut wrappers = Vec::new();
    let mut configs_kept = Vec::new();
    for (key, path) in installed::placed(env)? {
        if key.ends_with(".bin") {
            wrappers.push(path);
        } else if key.ends_with(".config") {
            configs_kept.push(path);
        }
    }
    let script = script();
    let mut argv = vec!["sh", "-c", &script, "lodge-clean"];
    argv.extend(wrappers.iter().map(String::as_str));
    let out = env.output(&argv, &ExecOpts::root())?;
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

    #[test]
    fn the_script_names_what_injection_owns_and_not_the_devcontainers_state() {
        let s = script();
        assert!(s.contains("'/opt/lodge/bin'"));
        assert!(s.contains("'/lodge'"));
        assert!(s.contains("find /usr/local/bin"));
        // Hook markers, the config hash and containerEnv stay.
        assert!(!s.contains("/opt/lodge/state"));
        assert!(!s.contains("devcontainer.hash"));
        assert!(!s.contains("lodge-env.sh"));
        assert!(!s.contains("rm -rf '/opt/lodge' "));
    }

    #[test]
    fn only_lodge_wrappers_are_removed() {
        let dir = crate::tmpfile::TempDir::new("wrappers").unwrap();
        let searched = dir.path().join("bin");
        let custom = dir.path().join("custom");
        std::fs::create_dir_all(&searched).unwrap();
        std::fs::create_dir_all(&custom).unwrap();
        let ours = searched.join("nvim");
        let theirs = searched.join("other");
        let recorded = custom.join("x");
        std::fs::write(&ours, "#!/bin/sh\nexec /opt/lodge/bin/nvim \"$@\"\n").unwrap();
        std::fs::write(&theirs, "#!/bin/sh\nexec /usr/bin/other \"$@\"\n").unwrap();
        std::fs::write(&recorded, "#!/bin/sh\nexec /opt/lodge/bin/x \"$@\"\n").unwrap();

        // `bin/` is searched; `custom/x`, outside it, is a recorded path.
        let dirs = crate::shell::shell_quote(&searched.to_string_lossy());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(remove_wrappers(&dirs))
            .arg("lodge-clean")
            .arg(&recorded)
            .output()
            .unwrap();
        assert!(out.status.success());
        let printed = String::from_utf8_lossy(&out.stdout);
        assert!(!ours.exists() && !recorded.exists(), "{printed}");
        assert!(theirs.exists(), "a wrapper that isn't lodge's was removed");
        assert!(
            printed.contains("nvim") && !printed.contains("other"),
            "{printed}"
        );
    }
}
