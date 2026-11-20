//! What lodge has already installed in a container, by content hash.
//!
//! With a hash per installed piece, a run can tell *unchanged* (do nothing, which
//! is cheap) from *changed* (update it). `skip_if_exists` still lets a user
//! protect something from being replaced once it is there.

use anyhow::Result;

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

/// Every recorded piece: its key (`<tool>.bin` or `<tool>.config`) and where it was
/// placed. Records from before paths were kept are left out.
pub(super) fn placed(env: &dyn Env) -> Vec<(String, String)> {
    let script = format!(
        "for f in {DIR}/*; do [ -f \"$f\" ] && printf '%s\\t%s\\n' \"${{f##*/}}\" \"$(sed -n 2p \"$f\")\"; done"
    );
    env.exec_capture(&["sh", "-c", &script])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, p)| !p.trim().is_empty())
        .map(|(k, p)| (k.to_string(), p.trim().to_string()))
        .collect()
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
    fn a_record_for_a_missing_install_does_not_count() {
        let env = FakeEnv::new();
        record(&env, "x.bin", "aa", "/usr/local/bin/x").unwrap();
        assert_eq!(decide(&env, "x.bin", "aa", false, false), Decision::Install);
    }
}
