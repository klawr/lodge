//! What lodge has already installed in a container, by content hash.
//!
//! With a hash per installed piece, a run can tell *unchanged* (do nothing, which
//! is cheap) from *changed* (update it). `skip_if_exists` still lets a user
//! protect something from being replaced once it is there.

use anyhow::Result;

use crate::runtime::Env;

const DIR: &str = "/opt/lodge/installed";

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

fn recorded(env: &dyn Env, key: &str) -> Option<String> {
    env.exec_capture(&["cat", &path(key)])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Records that `key` is installed with content `hash`.
pub(super) fn record(env: &dyn Env, key: &str, hash: &str) -> Result<()> {
    env.write_file(&path(key), format!("{hash}\n").as_bytes())
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
        record(&env, "nvim.bin", "aa").unwrap();
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
        record(&env, "nvim.bin", "aa").unwrap();
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
        record(&env, "x.bin", "aa").unwrap();
        assert_eq!(decide(&env, "x.bin", "aa", false, false), Decision::Install);
    }
}
