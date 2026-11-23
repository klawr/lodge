//! Where a registry login comes from, behind an interface so a credential helper
//! or a keyring is one more implementation, and a test hands in a fake.

use crate::local::Local;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

/// Several sources in order of preference: the first with a login answers. One
/// that is broken is an error, not a reason to try the next (a login that is
/// there but unreadable must not turn into an anonymous request).
pub struct FirstOf(pub Vec<Box<dyn Credentials>>);

impl Credentials for FirstOf {
    fn login(&self, registry: &str) -> Result<Option<Login>> {
        for source in &self.0 {
            if let Some(login) = source.login(registry)? {
                return Ok(Some(login));
            }
        }
        Ok(None)
    }
}

/// The logins `docker login` keeps: in its config file (`$DOCKER_CONFIG` or
/// `~/.docker`), or, when the file names a credential helper for the registry
/// (`credHelpers`) or for all (`credsStore`), in whatever that helper holds, which
/// is asked as docker asks it (`docker-credential-<name> get`).
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
    /// A helper for every registry without one of its own.
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    /// A helper per registry.
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
}

/// A helper may prompt (unlock a keyring), so it is given time; but not forever.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

/// What a helper answers `get` with.
#[derive(Deserialize)]
struct HelperAnswer {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

/// What a helper's `get` says when it has nothing for the registry, on stdout.
const NOT_FOUND: &str = "credentials not found";

/// What docker passes a helper as the server: Docker Hub under its historical
/// name, anything else as the host.
fn server_of(registry: &str) -> &str {
    if registry == "registry-1.docker.io" {
        "https://index.docker.io/v1/"
    } else {
        registry
    }
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

/// The name a registry is kept under: the host that serves its API, whatever the
/// user typed (`ghcr.io`, `https://ghcr.io/v1/`, Docker Hub as `docker.io`).
pub fn canonical_registry(typed: &str) -> String {
    let host = host_of(typed.trim());
    match host {
        "docker.io" | "index.docker.io" => "registry-1.docker.io".to_string(),
        host => host.to_string(),
    }
}

/// Asks a person for a login when a registry has refused the one lodge had, or
/// there was none. Behind an interface so the client never names a terminal, and
/// a test hands in a fake.
pub trait LoginPrompt: Send + Sync {
    /// A login typed in answer to `registry` refusing (`rejected`: lodge had one,
    /// and it was refused). `None` when nobody can be asked or they decline.
    fn ask(&self, registry: &str, rejected: bool) -> Result<Option<Login>>;

    /// A login that was asked for has been accepted: the place to offer keeping it.
    fn accepted(&self, registry: &str, login: &Login);
}

/// Nobody to ask: a daemon, a pipe, a test.
pub struct NoPrompt;

impl LoginPrompt for NoPrompt {
    fn ask(&self, _registry: &str, _rejected: bool) -> Result<Option<Login>> {
        Ok(None)
    }

    fn accepted(&self, _registry: &str, _login: &Login) {}
}

impl DockerConfig {
    /// Asks the helper `name` for the login of `registry`: `None` when it has none.
    fn ask_helper(&self, name: &str, registry: &str) -> Result<Option<Login>> {
        // The name becomes part of a program name, so it is a name and not a path.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            anyhow::bail!("the credential helper name {name:?} is not a plain name");
        }
        let program = format!("docker-credential-{name}");
        let out = self
            .local
            .run_program(
                &program,
                &["get"],
                server_of(registry).as_bytes(),
                HELPER_TIMEOUT,
            )
            .with_context(|| format!("asking {program} for the login of {registry}"))?;
        if !out.success {
            let said = String::from_utf8_lossy(&out.stdout);
            if said.trim().contains(NOT_FOUND) {
                return Ok(None);
            }
            let err = String::from_utf8_lossy(&out.stderr);
            let detail = [said.trim(), err.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("it failed");
            anyhow::bail!("{program} get for {registry} failed: {detail}");
        }
        let answer: HelperAnswer = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("reading what {program} answered for {registry}"))?;
        // An identity token is a refresh token for a token service's own login
        // flow, which lodge does not speak.
        if answer.username == "<token>" {
            anyhow::bail!(
                "{program} holds an identity token for {registry}, which lodge does not use; \
                 log in with a user name and a password or access token"
            );
        }
        Ok(Some(Login {
            username: answer.username,
            password: answer.secret,
        }))
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
        // The way docker chooses: a helper for this registry, else the general
        // one, else the file. A helper with nothing for the registry leaves the
        // file's own entry, if any, to answer.
        let helper = names
            .iter()
            .find_map(|n| config.cred_helpers.get(*n))
            .or(config.creds_store.as_ref());
        if let Some(helper) = helper
            && let Some(login) = self.ask_helper(helper, registry)?
        {
            return Ok(Some(login));
        }
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
    fn no_file_or_no_entry_is_anonymous() {
        let none = DockerConfig::new(Arc::new(FakeLocal::new().with_var("HOME", "/h")));
        assert!(none.login("ghcr.io").unwrap().is_none());
        assert!(
            config(r#"{"auths": {}}"#)
                .login("ghcr.io")
                .unwrap()
                .is_none()
        );
    }

    /// A machine whose config is `json` and whose helper `desktop` answers `get`.
    fn with_helper(json: &str, success: bool, answer: &str) -> (DockerConfig, Arc<FakeLocal>) {
        let local = Arc::new(
            FakeLocal::new()
                .with_var("HOME", "/home/me")
                .with_file("/home/me/.docker/config.json", json.as_bytes())
                .with_program("docker-credential-desktop get", success, answer.as_bytes()),
        );
        (DockerConfig::new(local.clone()), local)
    }

    const ANSWER: &str = r#"{"ServerURL": "ghcr.io", "Username": "kai", "Secret": "tok"}"#;

    #[test]
    fn a_helper_is_asked_with_the_server_on_its_input() {
        let (c, local) = with_helper(r#"{"credsStore": "desktop"}"#, true, ANSWER);
        let login = c.login("ghcr.io").unwrap().unwrap();
        assert_eq!(
            (login.username.as_str(), login.password.as_str()),
            ("kai", "tok")
        );
        assert_eq!(
            *local.asked.lock().unwrap(),
            vec![(
                "docker-credential-desktop get".to_string(),
                b"ghcr.io".to_vec()
            )]
        );
        // Docker Hub is asked for as docker asks for it.
        let (c, local) = with_helper(r#"{"credsStore": "desktop"}"#, true, ANSWER);
        c.login("registry-1.docker.io").unwrap();
        assert_eq!(
            local.asked.lock().unwrap()[0].1,
            b"https://index.docker.io/v1/".to_vec()
        );
    }

    #[test]
    fn a_helper_for_one_registry_beats_the_general_one_and_the_file() {
        let auth = STANDARD.encode("file:pw");
        let json = format!(
            r#"{{"credsStore": "other", "credHelpers": {{"ghcr.io": "desktop"}},
                "auths": {{"ghcr.io": {{"auth": "{auth}"}}}}}}"#
        );
        let (c, _) = with_helper(&json, true, ANSWER);
        assert_eq!(c.login("ghcr.io").unwrap().unwrap().username, "kai");
    }

    #[test]
    fn a_helper_with_nothing_for_the_registry_leaves_the_file_to_answer() {
        let auth = STANDARD.encode("file:pw");
        let json =
            format!(r#"{{"credsStore": "desktop", "auths": {{"ghcr.io": {{"auth": "{auth}"}}}}}}"#);
        let (c, _) = with_helper(&json, false, "credentials not found in native keychain\n");
        assert_eq!(c.login("ghcr.io").unwrap().unwrap().username, "file");
        let (c, _) = with_helper(
            r#"{"credsStore": "desktop"}"#,
            false,
            "credentials not found in native keychain\n",
        );
        assert!(c.login("ghcr.io").unwrap().is_none());
    }

    #[test]
    fn a_helper_that_fails_or_cannot_be_started_is_an_error_naming_it() {
        let (c, _) = with_helper(
            r#"{"credsStore": "desktop"}"#,
            false,
            "the keyring is locked",
        );
        let err = format!("{:#}", c.login("ghcr.io").unwrap_err());
        assert!(
            err.contains("docker-credential-desktop") && err.contains("locked"),
            "{err}"
        );
        // Not installed: the fake has no such program.
        let (c, _) = with_helper(r#"{"credsStore": "absent"}"#, true, ANSWER);
        let err = format!("{:#}", c.login("ghcr.io").unwrap_err());
        assert!(err.contains("docker-credential-absent"), "{err}");
        // An identity token is not a login lodge can use.
        let (c, _) = with_helper(
            r#"{"credsStore": "desktop"}"#,
            true,
            r#"{"Username": "<token>", "Secret": "t"}"#,
        );
        let err = format!("{:#}", c.login("ghcr.io").unwrap_err());
        assert!(err.contains("identity token"), "{err}");
    }

    #[test]
    fn a_helper_name_is_never_a_path_or_a_command() {
        for name in ["../evil", "a b", "x;rm", "", "/bin/sh"] {
            let (c, local) = with_helper(
                &format!(r#"{{"credsStore": {}}}"#, serde_json::json!(name)),
                true,
                ANSWER,
            );
            let err = format!("{:#}", c.login("ghcr.io").unwrap_err());
            assert!(err.contains("not a plain name"), "{name}: {err}");
            assert!(local.asked.lock().unwrap().is_empty(), "{name}");
        }
    }

    #[cfg(unix)]
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
    fn the_first_source_with_a_login_answers_and_a_broken_one_stops_the_rest() {
        use fake::FakeCredentials;
        let both = FirstOf(vec![
            Box::new(FakeCredentials::new().with("a.io", "first", "1")),
            Box::new(
                FakeCredentials::new()
                    .with("a.io", "second", "2")
                    .with("b.io", "second", "2"),
            ),
        ]);
        assert_eq!(both.login("a.io").unwrap().unwrap().username, "first");
        assert_eq!(both.login("b.io").unwrap().unwrap().username, "second");
        assert!(both.login("c.io").unwrap().is_none());
        let broken = FirstOf(vec![
            Box::new(FakeCredentials::new().broken("a.io", "locked")),
            Box::new(FakeCredentials::new().with("a.io", "second", "2")),
        ]);
        assert!(broken.login("a.io").is_err());
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
