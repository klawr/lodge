//! Where lodge keeps things inside a container: the one list of every path it
//! writes there, so injecting, `ldg clean` and the docs agree.
//!
//! `/opt/lodge/` holds what injection installs plus the devcontainer's own state.
//! `ldg clean` removes the first group ([`INJECTED`]) and keeps the second.

/// Relinked binaries; the wrappers `exec` them from here.
pub const BIN: &str = "/opt/lodge/bin";
/// Bundled shared libraries. Relinked binaries carry this as their search path,
/// so it is part of the bundle format.
pub const LIB: &str = "/opt/lodge/lib";
/// Hash of every library copied into [`LIB`].
pub const LIB_MANIFEST: &str = "/opt/lodge/lib/.manifest";
/// Where the dynamic linker is placed: relinked binaries name `/lodge/<ld>` as
/// their interpreter. The prefix is 7 bytes, matching `/lib64/`, so the
/// interpreter can be patched in place. Part of the bundle format.
pub const LINKER_DIR: &str = "/lodge";
/// A content hash and destination per installed binary and config.
pub const INSTALLED: &str = "/opt/lodge/installed";
/// The id of the bundle last injected.
pub const BUNDLE_ID: &str = "/opt/lodge/bundle";
/// The format version of this layout.
pub const VERSION: &str = "/opt/lodge/version";
/// Terminfo entries copied from the host.
pub const TERMINFO: &str = "/opt/lodge/terminfo";
/// Aliases, for login shells.
pub const PROFILE_ALIASES: &str = "/etc/profile.d/lodge.sh";
/// The container's own `PATH`, preserved for login shells.
pub const PROFILE_PATH: &str = "/etc/profile.d/lodge-path.sh";

/// One-shot markers: create-time hooks and feature installs that succeeded.
pub const MARKERS: &str = "/opt/lodge/state";
/// Fingerprint of the devcontainer inputs the container was created from.
pub const DEVCONTAINER_HASH: &str = "/opt/lodge/devcontainer.hash";
/// `containerEnv` and `remoteEnv`, for login shells.
pub const PROFILE_ENV: &str = "/etc/profile.d/lodge-env.sh";
/// Feature `containerEnv`, for login shells.
pub const PROFILE_FEATURES: &str = "/etc/profile.d/lodge-features.sh";
/// Where feature files are copied to run their `install.sh`.
pub const FEATURES: &str = "/tmp/lodge-features";

/// Prefix of the per-user secrets directory, `/dev/shm/lodge-secrets-<uid>`.
pub const SECRETS_PREFIX: &str = "/dev/shm/lodge-secrets-";

/// The secrets directory of the user with `uid`.
pub fn secrets_dir(uid: &str) -> String {
    format!("{SECRETS_PREFIX}{uid}")
}

/// Everything injection owns outright, which `ldg clean` removes (wrappers and
/// secrets are found separately). The devcontainer's state is not in it.
pub const INJECTED: &[&str] = &[
    BIN,
    LIB,
    LINKER_DIR,
    INSTALLED,
    BUNDLE_ID,
    TERMINFO,
    PROFILE_ALIASES,
    PROFILE_PATH,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_keeps_the_devcontainers_state() {
        for kept in [MARKERS, DEVCONTAINER_HASH, PROFILE_ENV, PROFILE_FEATURES] {
            assert!(
                !INJECTED.iter().any(|p| kept.starts_with(p)),
                "{kept} would be removed by `ldg clean`"
            );
        }
    }

    #[test]
    fn the_linker_dir_can_replace_lib64_in_place() {
        assert_eq!(format!("{LINKER_DIR}/").len(), "/lib64/".len());
    }
}
