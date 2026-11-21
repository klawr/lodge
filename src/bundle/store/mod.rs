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
//!
//! Writing is the other direction and a seam of its own, a [`BundleSink`], so a
//! machine that only uses bundles never needs a way to write one, and a fake of
//! either does not have to implement the other. A kind of place usually has both
//! (`file.rs`, `oci.rs`), and the reference that is read from is the one that is
//! written to.

mod file;
mod oci;

pub use file::{FileSink, FileStore};
pub use oci::{OciSink, OciStore};

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

/// Where a bundle can be written.
pub trait BundleSink {
    /// What a reference to this sink looks like, for messages.
    fn form(&self) -> &'static str;

    /// Whether `reference` is this sink's. Sinks must not overlap.
    fn handles(&self, reference: &str) -> bool;

    /// Where the tar is best built before `put`: a directory on the same
    /// filesystem as the destination, so placing it is a rename and a large
    /// bundle is not held in the scratch space as well. `None`: anywhere will do.
    fn staging(&self, reference: &str) -> Option<PathBuf>;

    /// Writes the bundle tar at `tar` as `reference`, replacing what was there.
    /// Returns the exact reference the bundle can be used by (a registry's tag
    /// becomes the digest it was written under).
    fn put(&self, reference: &str, tar: &Path) -> Result<String>;
}

/// The stores lodge can read a bundle from.
pub struct Stores(Vec<Box<dyn BundleStore>>);

impl Stores {
    pub fn new(stores: Vec<Box<dyn BundleStore>>) -> Self {
        Self(stores)
    }

    /// The store that owns `reference`.
    pub fn find(&self, reference: &str) -> Result<&dyn BundleStore> {
        owner(
            &self.0,
            reference,
            "read a bundle from",
            |s, r| s.handles(r),
            |s| s.form(),
        )
    }
}

/// The sinks lodge can write a bundle to.
pub struct Sinks(Vec<Box<dyn BundleSink>>);

impl Sinks {
    pub fn new(sinks: Vec<Box<dyn BundleSink>>) -> Self {
        Self(sinks)
    }

    /// The sink that owns `reference`.
    pub fn find(&self, reference: &str) -> Result<&dyn BundleSink> {
        owner(
            &self.0,
            reference,
            "write a bundle to",
            |s, r| s.handles(r),
            |s| s.form(),
        )
    }
}

/// The one of `kinds` that owns `reference`: none is an error that lists what
/// would work, and two is a wiring mistake that is reported and not decided by
/// order.
fn owner<'a, T: ?Sized>(
    kinds: &'a [Box<T>],
    reference: &str,
    what: &str,
    handles: impl Fn(&T, &str) -> bool,
    form: impl Fn(&T) -> &'static str,
) -> Result<&'a T> {
    let mut owners = kinds.iter().filter(|k| handles(k, reference));
    let Some(first) = owners.next() else {
        let forms: Vec<&str> = kinds.iter().map(|k| form(k)).collect();
        bail!("cannot {what} {reference:?}; it is {}", forms.join(" or "));
    };
    if let Some(second) = owners.next() {
        bail!(
            "both {} and {} claim the bundle {reference:?}",
            form(first),
            form(second)
        );
    }
    Ok(first.as_ref())
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

    /// What a sink was given: the reference, and the bytes.
    pub type Written = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// A sink for `fake://name` references that keeps what it is given.
    #[derive(Default)]
    pub struct FakeSink {
        pub written: Written,
    }

    impl FakeSink {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl BundleSink for FakeSink {
        fn form(&self) -> &'static str {
            "fake://<name>"
        }

        fn handles(&self, reference: &str) -> bool {
            reference.starts_with("fake://")
        }

        fn staging(&self, _reference: &str) -> Option<PathBuf> {
            None
        }

        fn put(&self, reference: &str, tar: &Path) -> Result<String> {
            self.written
                .lock()
                .unwrap()
                .push((reference.into(), std::fs::read(tar)?));
            Ok(format!("{reference}#exact"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeSink, FakeStore};
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
    fn a_reference_to_write_goes_to_the_sink_that_owns_it() {
        let sinks = Sinks::new(vec![Box::new(FileSink), Box::new(FakeSink::new())]);
        assert_eq!(sinks.find("fake://a").unwrap().form(), "fake://<name>");
        assert_eq!(sinks.find("out.tar").unwrap().form(), "a file path");
        let err = sinks.find("ftp://x/b.tar").err().unwrap().to_string();
        assert!(err.contains("cannot write a bundle to"), "{err}");
        assert!(err.contains("a file path or fake://<name>"), "{err}");
    }

    #[test]
    fn two_stores_claiming_a_reference_is_an_error_not_a_pick() {
        let both = Stores::new(vec![Box::new(FakeStore::new()), Box::new(FakeStore::new())]);
        let err = both.find("fake://a").err().unwrap().to_string();
        assert!(err.contains("both"), "{err}");
    }
}
