//! Where a bundle comes from: the seam between a reference the user wrote
//! (`~/lodge.tar`, `oci://ghcr.io/me/lodge:1`) and the bundle tar on this machine.
//!
//! A [`BundleStore`] owns one kind of reference. The user's reference goes to the
//! store that [`handles`](BundleStore::handles) it, so nothing else knows what
//! kinds exist: a new place bundles can be kept (an `https://` URL, a bucket) is
//! one more store, handed to [`Stores`] where `cmd::run` builds it. What comes out
//! is always a tar that `bundle::source` extracts into the cache, so injection
//! never sees where a bundle came from.
//!
//! Fetching is two steps so the cache can skip the expensive one:
//! [`identify`](BundleStore::identify) is cheap and names the content as it is
//! now, [`fetch`](BundleStore::fetch) gets it.

mod file;
mod oci;

pub use file::FileStore;
pub use oci::OciStore;

use crate::arch::Arch;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// What a reference points at right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Names the content, and is a safe directory name: the cache keeps the
    /// extracted bundle under it, so it changes exactly when the content does.
    pub key: String,
    /// The reference to fetch: what the user wrote, made exact (a tag becomes a
    /// digest), so what is fetched is what was identified.
    pub pinned: String,
}

pub trait BundleStore {
    /// What a reference to this store looks like, for messages that list the
    /// ways to name a bundle (`oci://<reference>`).
    fn form(&self) -> &'static str;

    /// Whether `reference` is this store's. Stores must not overlap.
    fn handles(&self, reference: &str) -> bool;

    /// What `reference` points at now. `arch` is the machine the bundle is for,
    /// `None` when it is not one lodge knows: a store with one bundle per machine
    /// needs it, a file does not.
    fn identify(&self, reference: &str, arch: Option<Arch>) -> Result<Identity>;

    /// The bundle's tar. It may be placed under `into` (an empty directory the
    /// caller removes) or already be where it is (a file): either way the path
    /// is returned and only read.
    fn fetch(&self, identity: &Identity, into: &Path) -> Result<PathBuf>;
}

/// The stores lodge can read a bundle from.
pub struct Stores(Vec<Box<dyn BundleStore>>);

impl Stores {
    pub fn new(stores: Vec<Box<dyn BundleStore>>) -> Self {
        Self(stores)
    }

    /// The store that owns `reference`.
    pub fn find(&self, reference: &str) -> Result<&dyn BundleStore> {
        let mut owners = self.0.iter().filter(|s| s.handles(reference));
        let Some(owner) = owners.next() else {
            let forms: Vec<&str> = self.0.iter().map(|s| s.form()).collect();
            bail!(
                "cannot read a bundle from {reference:?}; it is {}",
                forms.join(" or ")
            );
        };
        if let Some(other) = owners.next() {
            // A wiring mistake, not the user's: report it rather than pick one.
            bail!(
                "both {} and {} claim the bundle {reference:?}",
                owner.form(),
                other.form()
            );
        }
        Ok(owner.as_ref())
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// A store for `fake://name` references, with the bundles it is given. It
    /// records what was fetched, so a test can see that a cached bundle was not.
    #[derive(Default)]
    pub struct FakeStore {
        bundles: BTreeMap<String, (String, Vec<u8>)>,
        fetched: Arc<Mutex<Vec<String>>>,
    }

    impl FakeStore {
        pub fn new() -> Self {
            Self::default()
        }

        /// What gets fetched, for a test to read after the store is handed away.
        pub fn fetched(&self) -> Arc<Mutex<Vec<String>>> {
            self.fetched.clone()
        }

        /// `fake://name` is `tar`, whose key is `key`.
        pub fn with(mut self, name: &str, key: &str, tar: Vec<u8>) -> Self {
            self.bundles
                .insert(format!("fake://{name}"), (key.into(), tar));
            self
        }
    }

    impl BundleStore for FakeStore {
        fn form(&self) -> &'static str {
            "fake://<name>"
        }

        fn handles(&self, reference: &str) -> bool {
            reference.starts_with("fake://")
        }

        fn identify(&self, reference: &str, _arch: Option<Arch>) -> Result<Identity> {
            let (key, _) = self
                .bundles
                .get(reference)
                .ok_or_else(|| anyhow::anyhow!("no such fake bundle {reference}"))?;
            Ok(Identity {
                key: key.clone(),
                pinned: reference.into(),
            })
        }

        fn fetch(&self, identity: &Identity, into: &Path) -> Result<PathBuf> {
            self.fetched.lock().unwrap().push(identity.pinned.clone());
            let path = into.join("fake.tar");
            std::fs::write(&path, &self.bundles[&identity.pinned].1)?;
            Ok(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeStore;
    use super::*;

    fn stores() -> Stores {
        Stores::new(vec![
            Box::new(FileStore),
            Box::new(FakeStore::new().with("a", "x-1", vec![])),
        ])
    }

    #[test]
    fn a_reference_goes_to_the_store_that_owns_it() {
        let stores = stores();
        assert_eq!(stores.find("fake://a").unwrap().form(), "fake://<name>");
        assert_eq!(stores.find("./b.tar").unwrap().form(), "a file path");
        assert_eq!(stores.find("/abs/b.tar").unwrap().form(), "a file path");
    }

    #[test]
    fn an_unknown_scheme_says_what_would_work() {
        let err = stores().find("ftp://x/b.tar").err().unwrap().to_string();
        assert!(err.contains("ftp://x/b.tar"), "{err}");
        assert!(err.contains("a file path or fake://<name>"), "{err}");
    }

    #[test]
    fn two_stores_claiming_a_reference_is_an_error_not_a_pick() {
        let both = Stores::new(vec![Box::new(FakeStore::new()), Box::new(FakeStore::new())]);
        let err = both.find("fake://a").err().unwrap().to_string();
        assert!(err.contains("both"), "{err}");
    }
}
