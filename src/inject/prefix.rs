//! Choosing the prefix injection installs into.
//!
//! The user's home when it can hold programs, else a private directory in
//! `/tmp`. "Can hold programs" is tested the only reliable way: write a tiny
//! script there and run it, because a `noexec` mount (hardened hosts, some
//! volumes) refuses the run even when the write works. Everything is done as the
//! user, so what is chosen is somewhere that user can really use.

use anyhow::{Result, bail};

use crate::layout::Layout;
use crate::output::info;
use crate::runtime::{Acting, ContainerUser, Env, ExecOpts};
use crate::shell::script_argv;

/// `$1` is a directory; `$2` is `own` when the caller must own it (a shared
/// `/tmp` is not trusted otherwise). Makes the directory, refuses a symlink, and
/// checks that a script can be written there and run from there. Exits 0 only if
/// all of that works.
pub(super) const PROBE_SCRIPT: &str = r#"d=$1
mkdir -p "$d" 2>/dev/null || exit 1
[ -d "$d" ] && [ ! -L "$d" ] || exit 1
if [ "$2" = own ]; then
  [ -O "$d" ] && chmod 700 "$d" 2>/dev/null || exit 1
fi
t="$d/.probe.$$"
printf '#!/bin/sh\nexit 0\n' > "$t" 2>/dev/null || exit 1
chmod +x "$t" 2>/dev/null || { rm -f "$t"; exit 1; }
"$t" 2>/dev/null; r=$?
rm -f "$t"
exit $r"#;

/// The prefix to install into for `user`, made and checked. An error when
/// neither the home nor `/tmp` can hold one, or when the target can't be asked.
pub fn resolve(container: &dyn Env, user: &ContainerUser) -> Result<Layout> {
    let env = Acting::new(container, user);
    let preferred = Layout::preferred(user);
    if usable(&env, &preferred, false)? {
        return Ok(preferred);
    }
    let fallback = Layout::fallback(user);
    if usable(&env, &fallback, true)? {
        info!(
            "{} can't hold programs for {}, so lodge uses {} instead",
            user.home,
            user.name,
            fallback.prefix()
        );
        return Ok(fallback);
    }
    bail!(
        "lodge found nowhere to install for {}: it can't write and run programs in {} or in {}. \
         Give {} a home directory it can write to (lodge never uses root).",
        user.name,
        preferred.prefix(),
        fallback.prefix(),
        user.name
    )
}

fn usable(env: &dyn Env, layout: &Layout, own: bool) -> Result<bool> {
    let mode = if own { "own" } else { "any" };
    let out = env.output(
        &script_argv(PROBE_SCRIPT, &[layout.prefix(), mode]),
        &ExecOpts::default().repeatable(),
    )?;
    Ok(out.success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeEnv, Who};
    use std::os::unix::fs::PermissionsExt;
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

    fn probe(prefix: &str, mode: &str) -> Vec<String> {
        script_argv(PROBE_SCRIPT, &[prefix, mode])
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn argv(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_home_is_used_when_it_can_hold_programs() {
        let p = probe("/home/dev/.local/share/lodge", "any");
        let env = FakeEnv::new().with_output(&argv(&p), "");
        let layout = resolve(&env, &dev()).unwrap();
        assert_eq!(layout.prefix(), "/home/dev/.local/share/lodge");
        assert_eq!(env.ran_as(&argv(&p)), Some(Who::Named("dev".into())));
    }

    #[test]
    fn a_home_that_cannot_hold_programs_falls_back_to_a_private_tmp_directory() {
        let home = probe("/home/dev/.local/share/lodge", "any");
        let tmp = probe("/tmp/lodge-1000", "own");
        let env = FakeEnv::new()
            .with_failure(&argv(&home))
            .with_output(&argv(&tmp), "");
        assert_eq!(resolve(&env, &dev()).unwrap().prefix(), "/tmp/lodge-1000");
        assert_eq!(
            env.ran_as(&argv(&tmp)),
            Some(Who::Named("dev".into())),
            "even the fallback is probed as the user"
        );
    }

    #[test]
    fn nowhere_to_install_is_an_error_that_says_what_to_fix() {
        let env = FakeEnv::new()
            .with_failure(&argv(&probe("/home/dev/.local/share/lodge", "any")))
            .with_failure(&argv(&probe("/tmp/lodge-1000", "own")));
        let err = resolve(&env, &dev()).unwrap_err().to_string();
        assert!(
            err.contains("/home/dev/.local/share/lodge")
                && err.contains("/tmp/lodge-1000")
                && err.contains("never uses root"),
            "{err}"
        );
    }

    fn run(dir: &std::path::Path, mode: &str) -> bool {
        let dir = dir.to_string_lossy();
        let argv = script_argv(PROBE_SCRIPT, &[&dir, mode]);
        Command::new(argv[0])
            .args(&argv[1..])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn the_real_probe_accepts_a_directory_it_can_make_and_leaves_nothing_behind() {
        let base = crate::tmpfile::TempDir::new("prefix-ok").unwrap();
        let prefix = base.path().join("a/b/lodge");
        assert!(run(&prefix, "any"), "made with its parents");
        assert!(prefix.is_dir());
        assert_eq!(
            std::fs::read_dir(&prefix).unwrap().count(),
            0,
            "the probe file is removed"
        );
        assert!(run(&prefix, "any"), "and again");
    }

    #[test]
    fn the_real_probe_refuses_a_symlink_a_foreign_owner_and_an_unwritable_place() {
        let base = crate::tmpfile::TempDir::new("prefix-bad").unwrap();
        let real = base.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(!run(&link, "any"), "a symlink could point anywhere");
        assert!(!run(&link, "own"));

        let locked = base.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let writable = std::fs::File::create(locked.join(".t")).is_ok(); // root ignores modes
        if !writable {
            assert!(!run(&locked.join("lodge"), "any"));
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        // A private directory is made 0700.
        let own = base.path().join("own");
        assert!(run(&own, "own"));
        assert_eq!(
            std::fs::metadata(&own).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
