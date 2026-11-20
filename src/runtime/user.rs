//! Who commands run as, and who the session user is.

use anyhow::{Context, Result, bail};

use super::{Env, ExecOpts};

/// Who a command runs as.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum User<'a> {
    /// The environment's default user (a container's `USER`).
    #[default]
    Default,
    /// uid 0, regardless of the default. lodge needs this for setup that writes
    /// to root-owned locations (`/opt/lodge`, `/lodge`, `/etc/profile.d`).
    Root,
    Named(&'a str),
}

impl<'a> From<Option<&'a str>> for User<'a> {
    /// `remoteUser` and friends: a name, or `None` for the default user.
    fn from(name: Option<&'a str>) -> Self {
        name.map_or(User::Default, User::Named)
    }
}

/// The `sh -c` script behind [`ContainerUser::resolve`]: one line each for the
/// uid, the gid, the name (empty for a uid with no passwd entry) and `$HOME`.
pub(crate) const PROBE_SCRIPT: &str =
    "printf '%s\\n' \"$(id -u)\" \"$(id -g)\" \"$(id -un 2>/dev/null)\" \"$HOME\"";

/// The user sessions, hooks and injected files belong to, as the container
/// knows them. Resolved once and passed down, so every step agrees on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerUser {
    /// `remoteUser` as configured, or `None` for the container's default user.
    pub requested: Option<String>,
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The home directory. The engine sets `$HOME` from `/etc/passwd`, which
    /// needs no `getent` (missing in minimal images).
    pub home: String,
}

impl ContainerUser {
    /// Asks the container about `requested` (or its default user).
    pub fn resolve(env: &dyn Env, requested: Option<&str>) -> Result<Self> {
        let opts = ExecOpts {
            user: requested.into(),
            ..ExecOpts::default()
        };
        let out = env
            .output(&["sh", "-c", PROBE_SCRIPT], &opts)
            .context("asking the container who the user is")?;
        if !out.success {
            bail!(
                "could not look up the container user {}: {}",
                requested.unwrap_or("(default)"),
                out.stderr.trim()
            );
        }
        Self::parse(requested, &out.stdout).with_context(|| {
            format!(
                "unexpected answer about the container user: {:?}",
                out.stdout
            )
        })
    }

    fn parse(requested: Option<&str>, text: &str) -> Option<Self> {
        let mut lines = text.lines().map(str::trim);
        let uid: u32 = lines.next()?.parse().ok()?;
        let gid: u32 = lines.next()?.parse().ok()?;
        let name = match lines.next() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => requested.map_or_else(|| uid.to_string(), str::to_string),
        };
        let home = match lines.next() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ if uid == 0 => "/root".to_string(),
            _ => format!("/home/{name}"),
        };
        Some(Self {
            requested: requested.map(str::to_string),
            name,
            uid,
            gid,
            home,
        })
    }

    /// root, as a fallback when the container can't say.
    pub fn root() -> Self {
        Self {
            requested: None,
            name: "root".into(),
            uid: 0,
            gid: 0,
            home: "/root".into(),
        }
    }

    /// How to run a command as this user.
    pub fn exec_as(&self) -> User<'_> {
        self.requested.as_deref().into()
    }

    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    /// `uid:gid`, for `chown`.
    pub fn owner(&self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }

    /// `path` with a leading `~` expanded to this user's home.
    pub fn expand_home(&self, path: &str) -> String {
        let home = self.home.trim_end_matches('/');
        if path == "~" {
            home.to_string()
        } else if let Some(rest) = path.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            path.to_string()
        }
    }

    /// Whether `path` is the home directory or inside it.
    pub fn owns_path(&self, path: &str) -> bool {
        std::path::Path::new(path).starts_with(&self.home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    fn user(home: &str) -> ContainerUser {
        ContainerUser {
            requested: Some("vscode".into()),
            name: "vscode".into(),
            uid: 1000,
            gid: 1000,
            home: home.into(),
        }
    }

    #[test]
    fn the_probe_answers_every_question_at_once() {
        let env = FakeEnv::new().with_output(
            &["sh", "-c", PROBE_SCRIPT],
            "1000\n1001\nvscode\n/home/vscode\n",
        );
        let u = ContainerUser::resolve(&env, Some("vscode")).unwrap();
        assert_eq!(u, {
            let mut u = user("/home/vscode");
            u.gid = 1001;
            u
        });
        assert_eq!(u.owner(), "1000:1001");
        assert_eq!(u.exec_as(), User::Named("vscode"));
    }

    #[test]
    fn missing_names_and_homes_get_sensible_fallbacks() {
        let u = ContainerUser::parse(None, "1234\n1234\n\n\n").unwrap();
        assert_eq!((u.name.as_str(), u.home.as_str()), ("1234", "/home/1234"));
        let u = ContainerUser::parse(None, "0\n0\nroot\n\n").unwrap();
        assert_eq!(u.home, "/root");
        assert!(u.is_root());
        assert_eq!(u.exec_as(), User::Default);
        assert!(ContainerUser::parse(None, "who knows").is_none());
    }

    #[test]
    fn a_failed_probe_is_an_error() {
        let env = FakeEnv::new().with_failure(&["sh", "-c", PROBE_SCRIPT]);
        assert!(ContainerUser::resolve(&env, None).is_err());
    }

    #[test]
    fn expand_home_expands_tilde() {
        let u = user("/home/vscode/");
        assert_eq!(u.expand_home("~/.config/nvim"), "/home/vscode/.config/nvim");
        assert_eq!(u.expand_home("~"), "/home/vscode");
        assert_eq!(u.expand_home("/usr/local/bin/nvim"), "/usr/local/bin/nvim");
    }

    #[test]
    fn owns_path_compares_whole_components() {
        let u = user("/home/dev");
        assert!(u.owns_path("/home/dev/.config"));
        assert!(!u.owns_path("/home/dev2/.config"));
    }
}
