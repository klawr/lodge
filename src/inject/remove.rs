//! Taking an injection back out of a container (`ldg clean`).
//!
//! Removes what injection put there: the binaries, libraries and linker, the
//! wrappers (only ones that still point at lodge), the shell setup and any
//! placed secrets. Injected *configs* are left in place and reported, since the
//! user may have changed them in the container. The devcontainer's own state
//! (hook markers, the config hash, `containerEnv`) is kept, so a later `ldg up`
//! neither re-runs create hooks nor thinks the config drifted.

use anyhow::{Result, bail};

use super::installed;
use crate::bundle::{LODGE_BIN, LODGE_DIR, LODGE_LIB};
use crate::runtime::{Env, ExecOpts};

/// What [`remove`] did.
#[derive(Debug, Default)]
pub struct Removed {
    /// Wrapper scripts that were deleted.
    pub wrappers: Vec<String>,
    /// Injected config files and directories, left in place.
    pub configs_kept: Vec<String>,
}

/// The directories and files injection owns outright.
fn owned_paths() -> Vec<String> {
    vec![
        LODGE_BIN.to_string(),
        LODGE_LIB.to_string(),
        LODGE_DIR.to_string(),
        "/opt/lodge/installed".into(),
        "/opt/lodge/bundle".into(),
        "/opt/lodge/terminfo".into(),
        "/etc/profile.d/lodge.sh".into(),
        "/etc/profile.d/lodge-path.sh".into(),
    ]
}

/// Where wrappers usually live, searched as well: a container injected by an
/// older ldg has no recorded paths, and a wrapper may sit at a custom path.
const WRAPPER_DIRS: &str =
    "/usr/local/bin /usr/local/sbin /usr/bin /bin /root/.local/bin /home/*/.local/bin";

/// The `sh -c` script: `$@` are recorded wrapper paths. A file is removed only
/// if it is a lodge wrapper (small, and exec's into the injected binary); each
/// removed path is printed.
fn script() -> String {
    let owned: Vec<String> = owned_paths()
        .iter()
        .map(|p| crate::shell::shell_quote(p))
        .collect();
    format!(
        "marker={marker}; \
         for p in \"$@\" $(find {WRAPPER_DIRS} -maxdepth 1 -type f -size -2k 2>/dev/null); do \
           [ -f \"$p\" ] && grep -q \"$marker\" \"$p\" 2>/dev/null && rm -f \"$p\" && echo \"$p\"; \
         done; \
         rm -rf {owned} /dev/shm/lodge-secrets-*; true",
        marker = crate::shell::shell_quote(&format!("exec {LODGE_BIN}/")),
        owned = owned.join(" "),
    )
}

/// Removes the injection from `env`.
pub fn remove(env: &dyn Env) -> Result<Removed> {
    let mut wrappers = Vec::new();
    let mut configs_kept = Vec::new();
    for (key, path) in installed::placed(env) {
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
    fn the_script_keeps_devcontainer_state_and_only_removes_lodge_wrappers() {
        let s = script();
        assert!(s.contains("'/opt/lodge/bin'"));
        assert!(s.contains("'/lodge'"));
        assert!(s.contains("/dev/shm/lodge-secrets-*"));
        assert!(s.contains("marker='exec /opt/lodge/bin/'"));
        assert!(s.contains("find /usr/local/bin"));
        // Hook markers, the config hash and containerEnv stay.
        assert!(!s.contains("/opt/lodge/state"));
        assert!(!s.contains("devcontainer.hash"));
        assert!(!s.contains("lodge-env.sh"));
        assert!(!s.contains("rm -rf '/opt/lodge' "));
    }
}
