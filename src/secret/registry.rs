//! Registry logins kept on this machine, in its credential store: what
//! `ldg registry login` writes and what the registry client asks before docker's
//! own config. A login belongs to the machine and the person, so it is neither in
//! the config nor in a bundle.

use anyhow::{Result, bail};
use std::sync::Arc;

use crate::credstore::CredentialStore;
use crate::local::Local;
use crate::tools::{Credentials, Login, canonical_registry};

/// The store entry for a registry. The prefix keeps it apart from the entries a
/// `[[secret]]` names (those are plain names, which cannot contain a colon).
fn entry(registry: &str) -> String {
    format!("registry:{}", canonical_registry(registry))
}

/// Keeps `login` for `registry`, replacing what was there. Held as the user name
/// and the password on two lines, so a user name may not hold a line break.
pub fn save(store: &dyn CredentialStore, registry: &str, login: &Login) -> Result<()> {
    if canonical_registry(registry).is_empty() {
        bail!("{registry:?} is not a registry host");
    }
    if login.username.is_empty() || login.username.contains(['\n', '\r']) {
        bail!("the user name is empty or has a line break in it");
    }
    if login.password.is_empty() {
        bail!("the password is empty; nothing stored");
    }
    let value = format!("{}\n{}", login.username, login.password);
    store.set(&entry(registry), value.as_bytes())
}

/// Forgets the login for `registry`; whether there was one.
pub fn forget(store: &dyn CredentialStore, registry: &str) -> Result<bool> {
    store.remove(&entry(registry))
}

/// The logins `ldg registry login` keeps.
pub struct StoredLogins {
    local: Arc<dyn Local>,
}

impl StoredLogins {
    pub fn new(local: Arc<dyn Local>) -> Self {
        Self { local }
    }
}

impl Credentials for StoredLogins {
    fn login(&self, registry: &str) -> Result<Option<Login>> {
        // A machine with no credential store (a server without a Secret Service) has
        // no stored logins; that must not hide docker's own, which come after.
        let bytes = match self.local.store().get(&entry(registry)) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(e) => {
                crate::output::trace_line(&format!("no stored login for {registry}: {e:#}"));
                return Ok(None);
            }
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("the stored login for {registry} is not text"))?;
        let Some((username, password)) = text.split_once('\n') else {
            bail!(
                "the stored login for {registry} is damaged; run `ldg registry login {registry}` again"
            );
        };
        Ok(Some(Login {
            username: username.into(),
            password: password.into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::fake::FakeLocal;

    fn login(user: &str, password: &str) -> Login {
        Login {
            username: user.into(),
            password: password.into(),
        }
    }

    #[test]
    fn a_saved_login_is_read_back_for_that_registry_only() {
        let local = Arc::new(FakeLocal::new());
        save(local.store(), "ghcr.io", &login("kai", "tok:en")).unwrap();
        let logins = StoredLogins::new(local);
        let got = logins.login("ghcr.io").unwrap().unwrap();
        assert_eq!(
            (got.username.as_str(), got.password.as_str()),
            ("kai", "tok:en")
        );
        // A token goes only where it was meant.
        assert!(logins.login("registry-1.docker.io").unwrap().is_none());
        assert!(logins.login("ghcr.io:5000").unwrap().is_none());
    }

    #[test]
    fn docker_hub_is_one_registry_under_all_its_names() {
        let local = Arc::new(FakeLocal::new());
        save(local.store(), "docker.io", &login("u", "p")).unwrap();
        let logins = StoredLogins::new(local.clone());
        assert!(logins.login("registry-1.docker.io").unwrap().is_some());
        assert!(forget(local.store(), "index.docker.io").unwrap());
        assert!(logins.login("registry-1.docker.io").unwrap().is_none());
        assert!(!forget(local.store(), "index.docker.io").unwrap());
    }

    #[test]
    fn what_is_typed_as_a_registry_is_reduced_to_its_host() {
        let local = FakeLocal::new();
        save(local.store(), "https://ghcr.io/v1/", &login("u", "p")).unwrap();
        assert!(local.stored("registry:ghcr.io").is_some());
    }

    #[test]
    fn an_empty_or_line_broken_login_is_refused() {
        let local = FakeLocal::new();
        for (user, password) in [("", "p"), ("u", ""), ("u\nv", "p")] {
            assert!(save(local.store(), "ghcr.io", &login(user, password)).is_err());
        }
        assert!(save(local.store(), "", &login("u", "p")).is_err());
        assert!(local.stored("registry:ghcr.io").is_none());
    }

    #[test]
    fn a_damaged_entry_is_an_error_not_anonymous() {
        let local = Arc::new(FakeLocal::new().with_stored("registry:ghcr.io", b"no newline"));
        let err = format!(
            "{:#}",
            StoredLogins::new(local).login("ghcr.io").unwrap_err()
        );
        assert!(err.contains("damaged") && err.contains("ghcr.io"), "{err}");
    }

    #[test]
    fn a_store_that_cannot_be_used_means_no_stored_login() {
        let local = Arc::new(FakeLocal::new().with_unusable_store());
        assert!(StoredLogins::new(local).login("ghcr.io").unwrap().is_none());
    }
}
