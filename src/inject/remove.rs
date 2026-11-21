//! Taking an injection back out of a container (`ldg clean`).
//!
//! Removes what injection put there: the user's prefix (binaries, libraries, the
//! dynamic linker and the record) and the wrappers the config asked for, if they
//! still point at lodge. Placed secrets are [`crate::secret`]'s to remove.
//! Injected *configs* are left in place and reported, since the user may have
//! changed them in the container. The devcontainer's own state (hook markers, the
//! config hash, `containerEnv`) is kept, so a later `ldg up` neither re-runs create
//! hooks nor thinks the config drifted.

use anyhow::{Result, bail};

use super::git::UNSET_SCRIPT;
use super::state::Installed;
use crate::runtime::{Acting, ContainerUser, Env, ExecOpts};
use crate::shell::{script_argv, shell_quote};

/// What [`remove`] did.
#[derive(Debug, Default)]
pub struct Removed {
    /// Wrapper scripts that were deleted.
    pub wrappers: Vec<String>,
    /// Injected config files and directories, left in place.
    pub configs_kept: Vec<String>,
    /// Git `safe.directory` entries that were unset.
    pub safe_directories: Vec<String>,
}

/// The script: `$@` are recorded wrapper paths. Removes the ones that are lodge's
/// (they contain `marker`), printing each removed path, then the directory
/// `prefix`. Exits non-zero when anything could not be removed, after trying the
/// rest.
fn script(prefix: &str, marker: &str) -> String {
    format!(
        "status=0; marker={marker}; \
         for p in \"$@\"; do \
           [ -f \"$p\" ] && grep -q \"$marker\" \"$p\" 2>/dev/null || continue; \
           if rm -f \"$p\"; then echo \"$p\"; else status=1; fi; \
         done; \
         rm -rf {prefix} || status=1; \
         exit $status",
        marker = shell_quote(marker),
        prefix = shell_quote(prefix),
    )
}

/// Removes the injection `installed` describes for `user` from `container`. An
/// error when something could not be removed (what could be is gone all the
/// same).
pub fn remove(container: &dyn Env, user: &ContainerUser, installed: &Installed) -> Result<Removed> {
    let env = Acting::new(container, user);
    let layout = &installed.layout;
    // First: the record that lists these entries goes with the prefix below.
    if !installed.safe_directories.is_empty() {
        let dirs: Vec<&str> = installed
            .safe_directories
            .iter()
            .map(String::as_str)
            .collect();
        let out = env.output(
            &script_argv(UNSET_SCRIPT, &dirs),
            &ExecOpts::default().repeatable(),
        )?;
        if !out.success {
            bail!("unsetting git safe.directory failed: {}", out.stderr.trim());
        }
    }
    let marker = format!("exec {}/", layout.bin());
    let script = script(layout.prefix(), &marker);
    let recorded: Vec<&str> = installed.wrappers.iter().map(String::as_str).collect();
    let out = env.output(
        &script_argv(&script, &recorded),
        &ExecOpts::default().repeatable(),
    )?;
    if !out.success {
        bail!("removing lodge's files failed: {}", out.stderr.trim());
    }
    let mut wrappers: Vec<String> = out.stdout.lines().map(str::to_string).collect();
    wrappers.sort();
    wrappers.dedup();
    Ok(Removed {
        wrappers,
        configs_kept: installed.configs.clone(),
        safe_directories: installed.safe_directories.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;
    use crate::runtime::fake::{FakeEnv, Who};
    use std::path::Path;
    use std::process::Command;

    fn dev() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    /// Runs the script for real against `prefix`, with `recorded` as the recorded
    /// wrappers: whether it succeeded, and the paths it printed.
    fn exec(prefix: &Path, recorded: &[&Path], marker: &str) -> (bool, String) {
        let script = script(&prefix.to_string_lossy(), marker);
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
    fn the_script_names_the_prefix_and_not_the_devcontainers_state() {
        let layout = Layout::preferred(&dev());
        let s = script(layout.prefix(), &format!("exec {}/", layout.bin()));
        assert!(s.contains("'/home/dev/.local/share/lodge'"));
        // Hook markers, the config hash and containerEnv stay.
        assert!(!s.contains("/opt/lodge"));
        assert!(!s.contains("lodge-env.sh"));
    }

    #[test]
    fn a_recorded_wrapper_is_removed_only_while_it_still_points_at_the_prefix() {
        let dir = crate::tmpfile::TempDir::new("wrapper-mine").unwrap();
        let prefix = dir.path().join("lodge");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::write(prefix.join("bin/tool"), "x").unwrap();
        let marker = format!("exec {}/", prefix.join("bin").display());
        let lodges = dir.path().join("a");
        let mine = dir.path().join("b");
        std::fs::write(&lodges, format!("#!/bin/sh\n{marker}a \"$@\"\n")).unwrap();
        std::fs::write(&mine, "#!/bin/sh\necho the user replaced it\n").unwrap();
        let (ok, printed) = exec(&prefix, &[&lodges, &mine], &marker);
        assert!(ok);
        assert!(!lodges.exists() && !prefix.exists());
        assert!(mine.exists(), "a file the user took over is not lodge's");
        assert_eq!(printed.trim(), lodges.to_string_lossy());
    }

    #[test]
    fn a_failed_removal_fails_the_script_after_trying_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::tmpfile::TempDir::new("clean-fail").unwrap();
        let prefix = dir.path().join("prefix");
        std::fs::create_dir_all(prefix.join("inner")).unwrap();
        std::fs::write(prefix.join("inner/f"), "x").unwrap();
        let wrapper = dir.path().join("wrapper");
        std::fs::write(&wrapper, "exec /nothing/").unwrap();
        let set = |mode| std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(mode));
        set(0o500).unwrap();
        let (ok, printed) = exec(&prefix, &[&wrapper], "exec /nothing/");
        set(0o700).unwrap();
        if prefix.join("inner").exists() {
            // Not root: the read-only folder kept its contents.
            assert!(!ok, "a removal that failed was reported as success");
        }
        assert!(!wrapper.exists(), "the rest is removed all the same");
        assert_eq!(printed.trim(), wrapper.to_string_lossy());
    }

    #[test]
    fn cleaning_reports_the_kept_configs_and_never_runs_as_root() {
        let layout = Layout::preferred(&dev());
        let installed = Installed {
            layout: layout.clone(),
            bundle: "abc".into(),
            source: None,
            wrappers: vec![],
            configs: vec!["/home/dev/.config/nvim".into()],
            safe_directories: vec!["/w".into()],
        };
        let removal = script(layout.prefix(), &format!("exec {}/", layout.bin()));
        let env = FakeEnv::new()
            .with_output(&script_argv(&removal, &[]), "")
            .with_output(&script_argv(UNSET_SCRIPT, &["/w"]), "");
        let removed = remove(&env, &dev(), &installed).unwrap();
        assert_eq!(removed.safe_directories, ["/w"]);
        assert_eq!(removed.configs_kept, ["/home/dev/.config/nvim"]);
        assert!(removed.wrappers.is_empty());
        for (call, who) in env.calls() {
            assert_ne!(who, Who::Root, "{call:?} ran as root");
        }
    }
}
