//! What lodge has already installed in a container, by content hash.
//!
//! With a hash per installed piece, a run can tell *unchanged* (do nothing, which
//! is cheap) from *changed* (update it). `skip_if_exists` still lets a user
//! protect something from being replaced once it is there.

use anyhow::{Context, Result};

use crate::layout::INSTALLED as DIR;
use crate::runtime::Env;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Already installed with exactly this content.
    UpToDate,
    /// Present but different, and the user asked not to replace it.
    KeepExisting,
    Install,
}

/// What to do about the piece `key` whose bundle content has hash `hash`.
/// `present` is whether something is installed at its destination now.
pub(super) fn decide(
    env: &dyn Env,
    key: &str,
    hash: &str,
    present: bool,
    skip_if_exists: bool,
) -> Decision {
    if present && recorded(env, key).as_deref() == Some(hash) {
        Decision::UpToDate
    } else if present && skip_if_exists {
        Decision::KeepExisting
    } else {
        Decision::Install
    }
}

fn path(key: &str) -> String {
    format!("{DIR}/{key}")
}

// The hash is the record's first line (older records have only that).
fn recorded(env: &dyn Env, key: &str) -> Option<String> {
    // Unreadable counts as unrecorded: the piece is installed again, which is safe.
    env.read_file(&path(key))
        .ok()
        .flatten()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Records that `key` is installed at `placed_at` (its path in the container)
/// with content `hash`.
pub(super) fn record(env: &dyn Env, key: &str, hash: &str, placed_at: &str) -> Result<()> {
    env.write_file(&path(key), format!("{hash}\n{placed_at}\n").as_bytes())
}

/// The script behind [`placed`]: one `<key>\t<path>` line per record. Exits 0
/// when there are none, so a failure means the records could not be read.
fn placed_script() -> String {
    format!(
        "for f in {DIR}/*; do [ -f \"$f\" ] || continue; \
         printf '%s\\t%s\\n' \"${{f##*/}}\" \"$(sed -n 2p \"$f\")\"; done; exit 0"
    )
}

/// Every recorded piece: its key (`<tool>.bin` or `<tool>.config`) and where it was
/// placed. Records from before paths were kept are left out. An error when the
/// records can't be read: not knowing what is installed is not "nothing".
pub(super) fn placed(env: &dyn Env) -> Result<Vec<(String, String)>> {
    let out = env
        .exec_capture_as(crate::runtime::User::Root, &["sh", "-c", &placed_script()])
        .context("reading what lodge installed")?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, p)| !p.trim().is_empty())
        .map(|(k, p)| (k.to_string(), p.trim().to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn nothing_installed_means_install() {
        let env = FakeEnv::new();
        assert_eq!(
            decide(&env, "nvim.bin", "aa", false, false),
            Decision::Install
        );
        assert_eq!(
            decide(&env, "nvim.bin", "aa", false, true),
            Decision::Install
        );
    }

    #[test]
    fn the_same_content_is_up_to_date_whatever_skip_says() {
        let env = FakeEnv::new();
        record(&env, "nvim.bin", "aa", "/usr/local/bin/nvim").unwrap();
        assert_eq!(
            decide(&env, "nvim.bin", "aa", true, false),
            Decision::UpToDate
        );
        assert_eq!(
            decide(&env, "nvim.bin", "aa", true, true),
            Decision::UpToDate
        );
    }

    #[test]
    fn changed_content_is_installed_unless_the_user_protected_it() {
        let env = FakeEnv::new();
        record(&env, "nvim.bin", "aa", "/usr/local/bin/nvim").unwrap();
        assert_eq!(
            decide(&env, "nvim.bin", "bb", true, false),
            Decision::Install
        );
        assert_eq!(
            decide(&env, "nvim.bin", "bb", true, true),
            Decision::KeepExisting
        );
    }

    #[test]
    fn something_present_but_never_recorded_counts_as_changed() {
        // Installed by an older ldg, or by hand.
        let env = FakeEnv::new();
        assert_eq!(decide(&env, "x.bin", "aa", true, false), Decision::Install);
        assert_eq!(
            decide(&env, "x.bin", "aa", true, true),
            Decision::KeepExisting
        );
    }

    #[test]
    fn the_placed_script_lists_records_and_their_paths() {
        let dir = crate::tmpfile::TempDir::new("placed").unwrap();
        std::fs::write(dir.path().join("nvim.bin"), "aa\n/usr/local/bin/nvim\n").unwrap();
        std::fs::write(dir.path().join("old.bin"), "bb\n").unwrap();
        let script = placed_script().replace(DIR, &dir.path().to_string_lossy());
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "nvim.bin\t/usr/local/bin/nvim\nold.bin\t\n"
        );
        // An empty folder is no records, not a failure.
        std::fs::remove_file(dir.path().join("nvim.bin")).unwrap();
        std::fs::remove_file(dir.path().join("old.bin")).unwrap();
        assert!(
            std::process::Command::new("sh")
                .args(["-c", &script])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn records_that_cannot_be_read_are_an_error() {
        let env = FakeEnv::new().with_failure(&["sh", "-c", &placed_script()]);
        assert!(placed(&env).is_err());
        let env = FakeEnv::new().with_output(
            &["sh", "-c", &placed_script()],
            "nvim.bin\t/usr/local/bin/nvim\nold.bin\t\n",
        );
        assert_eq!(
            placed(&env).unwrap(),
            [("nvim.bin".to_string(), "/usr/local/bin/nvim".to_string())]
        );
    }

    #[test]
    fn a_record_for_a_missing_install_does_not_count() {
        let env = FakeEnv::new();
        record(&env, "x.bin", "aa", "/usr/local/bin/x").unwrap();
        assert_eq!(decide(&env, "x.bin", "aa", false, false), Decision::Install);
    }
}
