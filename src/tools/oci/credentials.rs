//! Where a registry login comes from, behind an interface so a credential helper
//! or a keyring is one more implementation, and a test hands in a fake.

use crate::local::Local;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A user name and password (or token) for a registry. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Login {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

pub trait Credentials: Send + Sync {
    /// The login for `registry` (the host that serves its API), `None` to go
    /// anonymously. An error means the source is broken, not that there is no login.
    fn login(&self, registry: &str) -> Result<Option<Login>>;
}

/// The logins `docker login` wrote into its config file (`$DOCKER_CONFIG` or
/// `~/.docker`). Only a login stored in the file itself is read: one kept by a
/// credential helper is not asked for, so that registry is reached anonymously.
pub struct DockerConfig {
    local: Arc<dyn Local>,
}

impl DockerConfig {
    pub fn new(local: Arc<dyn Local>) -> Self {
        Self { local }
    }

    fn path(&self) -> Option<PathBuf> {
        if let Some(dir) = self.local.var("DOCKER_CONFIG") {
            return Some(PathBuf::from(dir).join("config.json"));
        }
        let home = self.local.var("HOME")?;
        Some(PathBuf::from(home).join(".docker/config.json"))
    }
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    auths: BTreeMap<String, Entry>,
}

#[derive(Deserialize, Default)]
struct Entry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

/// The ways a config file names a registry: `host`, `https://host`,
/// `https://host/v1/`.
fn host_of(key: &str) -> &str {
    let key = key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
        .unwrap_or(key);
    key.split('/').next().unwrap_or(key)
}

/// Docker Hub is stored under another name than the host that serves it.
fn names_of(registry: &str) -> Vec<&str> {
    if registry == "registry-1.docker.io" {
        vec![registry, "index.docker.io", "docker.io"]
    } else {
        vec![registry]
    }
}

impl Credentials for DockerConfig {
    fn login(&self, registry: &str) -> Result<Option<Login>> {
        let Some(path) = self.path() else {
            return Ok(None);
        };
        let raw = match self.local.read_file(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let config: ConfigFile =
            serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let names = names_of(registry);
        let Some(entry) = config
            .auths
            .iter()
            .find(|(key, _)| names.contains(&host_of(key)))
            .map(|(_, e)| e)
        else {
            return Ok(None);
        };
        if let Some(auth) = entry.auth.as_deref().filter(|a| !a.is_empty()) {
            let decoded = STANDARD
                .decode(auth)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .with_context(|| {
                    format!(
                        "the login for {registry} in {} is not base64",
                        path.display()
                    )
                })?;
            let (username, password) = decoded.split_once(':').with_context(|| {
                format!(
                    "the login for {registry} in {} is not user:password",
                    path.display()
                )
            })?;
            return Ok(Some(Login {
                username: username.into(),
                password: password.into(),
            }));
        }
        Ok(match (&entry.username, &entry.password) {
            (Some(u), Some(p)) => Some(Login {
                username: u.clone(),
                password: p.clone(),
            }),
            _ => None,
        })
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// Logins by registry; an `Err` entry is a broken source.
    #[derive(Default)]
    pub struct FakeCredentials(BTreeMap<String, std::result::Result<Login, String>>);

    impl FakeCredentials {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with(mut self, registry: &str, user: &str, password: &str) -> Self {
            self.0.insert(
                registry.into(),
                Ok(Login {
                    username: user.into(),
                    password: password.into(),
                }),
            );
            self
        }

        pub fn broken(mut self, registry: &str, message: &str) -> Self {
            self.0.insert(registry.into(), Err(message.into()));
            self
        }
    }

    impl Credentials for FakeCredentials {
        fn login(&self, registry: &str) -> Result<Option<Login>> {
            match self.0.get(registry) {
                Some(Ok(l)) => Ok(Some(l.clone())),
                Some(Err(m)) => anyhow::bail!("{m}"),
                None => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::fake::FakeLocal;

    fn config(json: &str) -> DockerConfig {
        let local = FakeLocal::new()
            .with_var("HOME", "/home/me")
            .with_file("/home/me/.docker/config.json", json.as_bytes());
        DockerConfig::new(Arc::new(local))
    }

    #[test]
    fn a_stored_login_is_read_by_host_in_any_of_its_spellings() {
        let auth = STANDARD.encode("kai:pa:ss");
        for key in ["ghcr.io", "https://ghcr.io", "https://ghcr.io/v1/"] {
            let c = config(&format!(
                r#"{{"auths": {{"{key}": {{"auth": "{auth}"}}}}}}"#
            ));
            let login = c.login("ghcr.io").unwrap().unwrap();
            assert_eq!(login.username, "kai", "{key}");
            assert_eq!(login.password, "pa:ss", "{key}");
        }
    }

    #[test]
    fn docker_hub_is_found_under_its_other_names() {
        let c = config(
            r#"{"auths": {"https://index.docker.io/v1/": {"username": "u", "password": "p"}}}"#,
        );
        assert!(c.login("registry-1.docker.io").unwrap().is_some());
        assert!(c.login("ghcr.io").unwrap().is_none());
    }

    #[test]
    fn no_file_no_entry_or_a_helper_is_anonymous() {
        let none = DockerConfig::new(Arc::new(FakeLocal::new().with_var("HOME", "/h")));
        assert!(none.login("ghcr.io").unwrap().is_none());
        let helper = config(r#"{"credsStore": "desktop", "auths": {"ghcr.io": {}}}"#);
        assert!(helper.login("ghcr.io").unwrap().is_none());
    }

    #[test]
    fn docker_config_wins_over_home_and_a_broken_file_is_an_error() {
        let local = FakeLocal::new()
            .with_var("DOCKER_CONFIG", "/dc")
            .with_var("HOME", "/h")
            .with_file("/dc/config.json", b"{not json");
        let err = DockerConfig::new(Arc::new(local))
            .login("x.io")
            .unwrap_err();
        assert!(format!("{err:#}").contains("/dc/config.json"), "{err:#}");
    }

    #[test]
    fn a_login_never_prints_its_password() {
        let l = Login {
            username: "u".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{l:?}").contains("hunter2"));
    }
}
