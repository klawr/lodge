//! One-shot markers: create-time hooks and feature installs record here that
//! they succeeded, so they run once *successfully* and a failure retries on the
//! next `ldg up`. Kept in the container, so a recreated container starts clean.

use anyhow::Result;

use crate::layout::MARKERS;
use crate::runtime::Env;

/// Whether the one-shot task `key` already completed in `env`; an error when
/// the container can't be asked, which callers must not read as "not yet".
pub(super) fn done(env: &dyn Env, key: &str) -> Result<bool> {
    env.exists(&path(key))
}

/// Records that the one-shot task `key` completed.
pub(super) fn mark_done(env: &dyn Env, key: &str) -> Result<()> {
    env.write_file(&path(key), b"")
}

fn path(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{MARKERS}/{safe}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn markers_make_hooks_run_once() {
        let env = FakeEnv::new();
        assert!(!done(&env, "postCreate").unwrap());
        mark_done(&env, "postCreate").unwrap();
        assert!(done(&env, "postCreate").unwrap());
        assert!(!done(&env, "onCreate").unwrap());
    }

    #[test]
    fn keys_are_made_path_safe() {
        let env = FakeEnv::new();
        mark_done(&env, "ghcr.io/x/feat:1").unwrap();
        assert!(env.file_exists("/opt/lodge/state/ghcr.io_x_feat_1"));
    }

    #[test]
    fn a_marker_that_cannot_be_checked_is_an_error_not_missing() {
        let env = FakeEnv::new().with_failure(&crate::shell::script_argv(
            crate::runtime::EXISTS_SCRIPT,
            &["/opt/lodge/state/onCreate"],
        ));
        assert!(done(&env, "onCreate").is_err());
    }

    #[test]
    fn a_marker_from_an_earlier_run_is_respected() {
        let env = FakeEnv::new().with_file("/opt/lodge/state/onCreate", "");
        assert!(done(&env, "onCreate").unwrap());
    }
}
