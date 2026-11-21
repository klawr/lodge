//! Who commands run as, and who the session user is.

use anyhow::{Context, Result, bail};
use std::sync::OnceLock;

use super::{Container, Env, ExecOpts};

/// Who a command runs as.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum User<'a> {
    /// The environment's default user (a container's `USER`).
    #[default]
    Default,
    /// uid 0, regardless of the default. Only provisioning asks for it, through
    /// the engine (a home to fix, a feature to install); what lodge does for a
    /// user never does.
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
        let out = env
            .output(
                &["sh", "-c", PROBE_SCRIPT],
                &ExecOpts::as_user(requested.into()),
            )
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

    /// The user as a container knows them *without running anything in it*, for a
    /// stopped container: `requested` (or the image's configured user, or root)
    /// looked up in its `/etc/passwd`, read through the engine. `None` when the
    /// passwd file has no such user.
    pub fn from_stored(container: &dyn Container, requested: Option<&str>) -> Result<Option<Self>> {
        let configured = container.configured_user();
        let wanted = requested
            .map(str::to_string)
            .or(configured)
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "root".to_string());
        // The image's setting may be `user:group` or `uid:gid`.
        let wanted = wanted.split(':').next().unwrap_or("root");
        let passwd = container
            .read_stored_file("/etc/passwd")?
            .unwrap_or_default();
        Ok(Self::from_passwd(requested, wanted, &passwd))
    }

    /// `wanted` (a name or a uid) in the text of a passwd file.
    fn from_passwd(requested: Option<&str>, wanted: &str, passwd: &str) -> Option<Self> {
        for fields in passwd.lines().map(|l| l.split(':').collect::<Vec<_>>()) {
            let [name, _, uid, gid, _, home, ..] = fields[..] else {
                continue;
            };
            if name == wanted || uid == wanted {
                return Some(Self {
                    requested: requested.map(str::to_string),
                    name: name.to_string(),
                    uid: uid.parse().ok()?,
                    gid: gid.parse().ok()?,
                    home: home.to_string(),
                });
            }
        }
        // A uid with no passwd entry, as the probe would have reported it.
        let uid: u32 = wanted.parse().ok()?;
        Self::parse(requested, &format!("{uid}\n{uid}\n\n\n"))
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

    /// How to run a command as this user. uid 0 is root whatever name it was asked
    /// for by, so that is how it is run.
    pub fn exec_as(&self) -> User<'_> {
        if self.is_root() {
            User::Root
        } else {
            self.requested.as_deref().into()
        }
    }

    pub fn is_root(&self) -> bool {
        self.uid == 0
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
}

/// What an endpoint that cannot switch user (a plain host, a pod: no `-u`) lets
/// lodge act as: the user it connects as, and nobody else. It asks who that is
/// once, the first time anyone else is named.
pub struct LoginOnly {
    /// What the endpoint is called in a message: "a plain host".
    what: &'static str,
    login: OnceLock<Option<ContainerUser>>,
}

impl LoginOnly {
    pub fn new(what: &'static str) -> Self {
        Self {
            what,
            login: OnceLock::new(),
        }
    }

    /// Fails unless `user` is the one `env` lets lodge act as: the user it
    /// connects as, or nobody in particular. Root only if that is who connects.
    pub fn check(&self, env: &dyn Env, user: User) -> Result<()> {
        if matches!(user, User::Default) {
            return Ok(());
        }
        let login = self
            .login
            .get_or_init(|| ContainerUser::resolve(env, None).ok())
            .as_ref()
            .with_context(|| {
                format!(
                    "could not tell which user {} connects as, so cannot act as another",
                    self.what
                )
            })?;
        let allowed = match user {
            User::Default => true,
            User::Root => login.is_root(),
            User::Named(name) => name == login.name || login.requested.as_deref() == Some(name),
        };
        if !allowed {
            bail!(
                "{} is acted on as the user it connects as ({}), not as {}",
                self.what,
                login.name,
                match user {
                    User::Root => "root",
                    User::Named(name) => name,
                    User::Default => "the default user",
                }
            );
        }
        Ok(())
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
        assert_eq!((u.uid, u.gid), (1000, 1001));
        assert_eq!(u.exec_as(), User::Named("vscode"));
    }

    #[test]
    fn missing_names_and_homes_get_sensible_fallbacks() {
        let u = ContainerUser::parse(None, "1234\n1234\n\n\n").unwrap();
        assert_eq!((u.name.as_str(), u.home.as_str()), ("1234", "/home/1234"));
        let u = ContainerUser::parse(None, "0\n0\nroot\n\n").unwrap();
        assert_eq!(u.home, "/root");
        assert!(u.is_root());
        assert_eq!(
            u.exec_as(),
            User::Root,
            "uid 0 is run as root, not as 'default'"
        );
        assert!(ContainerUser::parse(None, "who knows").is_none());
    }

    #[test]
    fn a_stopped_containers_user_is_found_in_its_passwd_file() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nnode:x:1000:1001::/home/node:/bin/bash\n";
        let by_name = ContainerUser::from_passwd(Some("node"), "node", passwd).unwrap();
        assert_eq!(
            (by_name.uid, by_name.gid, by_name.home.as_str()),
            (1000, 1001, "/home/node")
        );
        assert_eq!(by_name.requested.as_deref(), Some("node"));
        let by_uid = ContainerUser::from_passwd(None, "1000", passwd).unwrap();
        assert_eq!(by_uid.name, "node");
        assert_eq!(by_uid.requested, None);
        let root = ContainerUser::from_passwd(None, "root", passwd).unwrap();
        assert!(root.is_root() && root.home == "/root");
        // A uid nobody lists still gets the probe's fallbacks; a name nobody lists
        // is not guessed.
        let ghost = ContainerUser::from_passwd(None, "4242", passwd).unwrap();
        assert_eq!((ghost.uid, ghost.home.as_str()), (4242, "/home/4242"));
        assert!(ContainerUser::from_passwd(None, "nobody", passwd).is_none());
    }

    #[test]
    fn the_stored_user_follows_the_images_configured_user_and_defaults_to_root() {
        use crate::runtime::Runtime;
        use crate::runtime::fake::FakeRuntime;
        let passwd = "root:x:0:0::/root:/bin/sh\napp:x:1234:1234::/srv/app:/bin/sh\n";
        let rt = FakeRuntime::new(FakeEnv::new)
            .with_container("c", FakeEnv::new().with_file("/etc/passwd", passwd));
        let c = rt.container("c");
        // No requested user and no configured one: root.
        let u = ContainerUser::from_stored(c.as_ref(), None)
            .unwrap()
            .unwrap();
        assert!(u.is_root());
        // A requested user is looked up.
        let u = ContainerUser::from_stored(c.as_ref(), Some("app"))
            .unwrap()
            .unwrap();
        assert_eq!(u.home, "/srv/app");
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
}
