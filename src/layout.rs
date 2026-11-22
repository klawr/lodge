//! Where lodge keeps things inside a target: the one place that knows every path
//! it writes there, so injecting, `ldg clean` and the docs agree.
//!
//! Two kinds of path, kept apart (plus the secrets directory):
//!
//! - **The user's prefix** ([`Layout`]): everything *injection* installs, in one
//!   directory the session user owns, `~/.local/share/lodge` (or, when the home
//!   can't hold programs, `/tmp/lodge-<uid>`). Nothing outside it is written,
//!   apart from the configs and wrappers the user asked for, so it needs no root
//!   and `ldg clean` is one `rm -rf`.
//! - **Provisioning state**: what lodge records while *creating* a devcontainer
//!   (hook markers, the config hash, `containerEnv` for login shells). It is
//!   written through the engine as root, at creation, under `/opt/lodge`, and is
//!   the devcontainer's, not the user's.

use std::path::Path;

use crate::runtime::ContainerUser;

/// Where the prefix goes under a user's home.
const PREFIX_IN_HOME: &str = ".local/share/lodge";

/// The directory injection installs into, and the paths inside it. A bundle
/// holds no prefix: its binaries find their libraries relative to themselves and
/// are given their interpreter's path when injected, so the same bundle fits any
/// prefix. `bin/` and `lib/` must stay siblings (see
/// [`crate::bundle::manifest::ORIGIN_RPATH`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    prefix: String,
}

impl Layout {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into().trim_end_matches('/').to_string(),
        }
    }

    /// The prefix to use when it can: inside the user's home.
    pub fn preferred(user: &ContainerUser) -> Self {
        Self::new(format!(
            "{}/{PREFIX_IN_HOME}",
            user.home.trim_end_matches('/')
        ))
    }

    /// The prefix to fall back on when the home can't hold one (no home, not
    /// writable, or mounted `noexec`): a private directory in `/tmp`.
    pub fn fallback(user: &ContainerUser) -> Self {
        Self::new(format!("/tmp/lodge-{}", user.uid))
    }

    /// Every prefix injection may have used for `user`, in the order it prefers
    /// them: how to find where it was installed.
    pub fn candidates(user: &ContainerUser) -> [Self; 2] {
        [Self::preferred(user), Self::fallback(user)]
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Relinked binaries; sessions put this first on `PATH`.
    pub fn bin(&self) -> String {
        format!("{}/bin", self.prefix)
    }

    /// The installed binary of `tool`.
    pub fn bin_of(&self, tool: &str) -> String {
        format!("{}/{tool}", self.bin())
    }

    /// Bundled shared libraries and the dynamic linker. Binaries find them
    /// through their own relative search path.
    pub fn lib(&self) -> String {
        format!("{}/lib", self.prefix)
    }

    /// One bundled library, or the dynamic linker, which binaries name as their
    /// interpreter.
    pub fn lib_file(&self, name: &str) -> String {
        format!("{}/{name}", self.lib())
    }

    /// What the last injection recorded: the bundle's id, and what it placed
    /// outside the prefix. Written last, so its presence means the injection
    /// finished.
    pub fn record(&self) -> String {
        format!("{}/injected", self.prefix)
    }

    /// Terminfo entries copied from the host, for [`TERMINFO_DIRS`](
    /// https://man7.org/linux/man-pages/man5/terminfo.5.html).
    pub fn terminfo(&self) -> String {
        format!("{}/terminfo", self.prefix)
    }
}

/// `path` as the config writes it, and as it is meant in a container: `~/…` when
/// it is under `home` (the container's user has a home of their own), else the
/// path as it is. This is the place a `[[data]]` entry without a `target` goes
/// to: the same place as here.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if home != Path::new("/") => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

// ── provisioning state: the devcontainer's own, root-owned ───────────────────

/// One-shot markers: create-time hooks and feature installs that succeeded.
pub const MARKERS: &str = "/opt/lodge/state";
/// Fingerprint of the devcontainer inputs the container was created from.
pub const DEVCONTAINER_HASH: &str = "/opt/lodge/devcontainer.hash";
/// `containerEnv` and `remoteEnv`, for login shells.
pub const PROFILE_ENV: &str = "/etc/profile.d/lodge-env.sh";
/// Feature `containerEnv`, for login shells: one profile per installed feature,
/// so retrying one feature never loses another's.
pub fn profile_feature(feature_id: &str) -> String {
    format!("/etc/profile.d/lodge-feature-{feature_id}.sh")
}
/// Where feature files are copied to run their `install.sh`.
pub const FEATURES: &str = "/tmp/lodge-features";

// ── secrets ──────────────────────────────────────────────────────────────────

/// The secrets directory of the user with `uid`: `/dev/shm/lodge-secrets-<uid>`,
/// in memory and private to that user.
pub fn secrets_dir(uid: &str) -> String {
    format!("/dev/shm/lodge-secrets-{uid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(home: &str, uid: u32) -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid,
            gid: uid,
            home: home.into(),
        }
    }

    #[test]
    fn the_prefix_is_in_the_users_home_and_every_path_is_inside_it() {
        let l = Layout::preferred(&user("/home/dev/", 1000));
        assert_eq!(l.prefix(), "/home/dev/.local/share/lodge");
        for path in [
            l.bin(),
            l.bin_of("nvim"),
            l.lib(),
            l.lib_file("ld.so"),
            l.record(),
            l.terminfo(),
        ] {
            assert!(path.starts_with("/home/dev/.local/share/lodge/"), "{path}");
        }
        assert_eq!(l.bin_of("nvim"), "/home/dev/.local/share/lodge/bin/nvim");
    }

    #[test]
    fn a_path_keeps_its_place_and_only_the_home_is_relative() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me/.vimrc"), home), "~/.vimrc");
        assert_eq!(
            tilde(Path::new("/usr/local/share/nvim"), home),
            "/usr/local/share/nvim"
        );
        // A root home is every path's prefix, and no place of its own.
        assert_eq!(
            tilde(Path::new("/usr/bin/rg"), Path::new("/")),
            "/usr/bin/rg"
        );
    }

    #[test]
    fn root_gets_a_prefix_in_its_own_home_like_anyone_else() {
        let root = ContainerUser::root();
        assert_eq!(
            Layout::preferred(&root).prefix(),
            "/root/.local/share/lodge"
        );
    }

    #[test]
    fn the_fallback_is_private_to_the_uid() {
        assert_eq!(
            Layout::fallback(&user("/nonexistent", 1234)).prefix(),
            "/tmp/lodge-1234"
        );
    }

    #[test]
    fn bin_and_lib_are_siblings_as_the_bundles_relative_search_path_needs() {
        let l = Layout::new("/p/");
        assert_eq!(l.prefix(), "/p");
        assert_eq!(l.bin(), "/p/bin");
        assert_eq!(l.lib(), "/p/lib");
    }

    #[test]
    fn provisioning_state_is_not_in_the_users_prefix() {
        let l = Layout::preferred(&user("/home/dev", 1000));
        for path in [MARKERS, DEVCONTAINER_HASH, PROFILE_ENV, FEATURES] {
            assert!(!path.starts_with(l.prefix()), "{path}");
        }
    }
}
