//! The client's own place for secret values: the operating system's credential
//! store (Windows Credential Manager, the macOS Keychain, the Secret Service on
//! Linux). A `[[secret]]` that says `from = { store = "name" }` is read from here,
//! so the config only ever names a secret and the value is in neither the config
//! nor a bundle.
//!
//! This is the one place that knows about the OS store, behind [`CredentialStore`]
//! on the [`Local`](crate::local::Local) seam, with a fake for tests. The store is
//! per user and per machine: a value set on one client is not on another.

use anyhow::{Context, Result, bail};

/// Where a client keeps secret values by name. What it holds is a secret: the
/// callers wrap it as one at once ([`crate::secret::Secret`]) and print none of it.
pub trait CredentialStore: Send + Sync {
    /// The value stored under `name`; `None` when there is none. An error when the
    /// store itself can't be used, which is not "none".
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>>;

    /// Stores `value` under `name`, replacing what was there.
    fn set(&self, name: &str, value: &[u8]) -> Result<()>;

    /// Removes `name`; whether there was anything to remove.
    fn remove(&self, name: &str) -> Result<bool>;
}

/// What the entries are filed under in the OS store.
const SERVICE: &str = "lodge";

/// The operating system's credential store.
pub struct OsCredentialStore;

impl OsCredentialStore {
    fn entry(name: &str) -> Result<keyring::Entry> {
        if let Err(e) = keyring::Entry::store_status() {
            bail!(
                "no credential store is available here ({e}); on Linux that is a Secret \
                 Service (GNOME Keyring, KeePassXC, …) running in your session. Use an \
                 `env`, `file` or `command` source instead"
            );
        }
        keyring::Entry::new(SERVICE, name).with_context(|| format!("opening store entry {name}"))
    }
}

impl CredentialStore for OsCredentialStore {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match Self::entry(name)?.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading store entry {name}")),
        }
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<()> {
        Self::entry(name)?
            .set_secret(value)
            .with_context(|| format!("writing store entry {name}"))
    }

    fn remove(&self, name: &str) -> Result<bool> {
        match Self::entry(name)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing store entry {name}")),
        }
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A store in memory, or one that cannot be used.
    #[derive(Default)]
    pub struct FakeCredentialStore {
        entries: Mutex<BTreeMap<String, Vec<u8>>>,
        unavailable: bool,
    }

    impl FakeCredentialStore {
        pub fn new() -> Self {
            Self::default()
        }

        /// A store that fails every call, as one with no Secret Service does.
        pub fn unavailable() -> Self {
            Self {
                unavailable: true,
                ..Self::default()
            }
        }

        pub fn with(self, name: &str, value: &[u8]) -> Self {
            self.entries
                .lock()
                .unwrap()
                .insert(name.into(), value.to_vec());
            self
        }

        /// What is stored under `name`, for a test to look at.
        pub fn value(&self, name: &str) -> Option<Vec<u8>> {
            self.entries.lock().unwrap().get(name).cloned()
        }

        fn check(&self) -> Result<()> {
            if self.unavailable {
                bail!("no credential store is available here");
            }
            Ok(())
        }
    }

    impl CredentialStore for FakeCredentialStore {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            self.check()?;
            Ok(self.value(name))
        }

        fn set(&self, name: &str, value: &[u8]) -> Result<()> {
            self.check()?;
            self.entries
                .lock()
                .unwrap()
                .insert(name.into(), value.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<bool> {
            self.check()?;
            Ok(self.entries.lock().unwrap().remove(name).is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeCredentialStore;
    use super::*;

    #[test]
    fn a_value_is_stored_replaced_read_and_removed() {
        let store = FakeCredentialStore::new();
        assert!(store.get("tok").unwrap().is_none());
        store.set("tok", b"one").unwrap();
        store.set("tok", b"two").unwrap();
        assert_eq!(store.get("tok").unwrap().unwrap(), b"two");
        assert!(store.remove("tok").unwrap());
        assert!(!store.remove("tok").unwrap(), "nothing left to remove");
        assert!(store.get("tok").unwrap().is_none());
    }

    #[test]
    fn a_store_that_cannot_be_used_is_an_error_not_an_empty_one() {
        let store = FakeCredentialStore::unavailable();
        assert!(store.get("tok").is_err());
        assert!(store.set("tok", b"v").is_err());
        assert!(store.remove("tok").is_err());
    }

    /// Against the real credential store of the machine this runs on, which a CI
    /// box has none of: run by hand with `cargo test -- --ignored os_store`.
    #[test]
    #[ignore = "needs an OS credential store"]
    fn os_store_round_trips_a_value() {
        let name = format!("lodge_test_{}", std::process::id());
        let store = OsCredentialStore;
        store.set(&name, b"value-1").unwrap();
        assert_eq!(store.get(&name).unwrap().unwrap(), b"value-1");
        assert!(store.remove(&name).unwrap());
        assert!(store.get(&name).unwrap().is_none());
    }
}
