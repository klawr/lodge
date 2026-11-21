//! A bundle that is a file on this machine.

use super::{BundleStore, Identity};
use crate::arch::Arch;
use crate::bundle::hash::hash_path;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Every reference without a scheme is a path.
pub struct FileStore;

impl BundleStore for FileStore {
    fn form(&self) -> &'static str {
        "a file path"
    }

    fn handles(&self, reference: &str) -> bool {
        !reference.contains("://")
    }

    /// The key is the hash of the file, so a replaced file is a new bundle.
    fn identify(&self, reference: &str, _arch: Option<Arch>) -> Result<Identity> {
        let key = hash_path(Path::new(reference))
            .with_context(|| format!("reading bundle {reference}"))?;
        Ok(Identity {
            key: format!("f-{key}"),
            pinned: reference.into(),
        })
    }

    /// The file is the tar: nothing is copied.
    fn fetch(&self, identity: &Identity, _into: &Path) -> Result<PathBuf> {
        Ok(PathBuf::from(&identity.pinned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    #[test]
    fn the_key_follows_the_content_of_the_file() {
        let dir = TempDir::new("file-store").unwrap();
        let file = dir.path().join("b.tar");
        std::fs::write(&file, "one").unwrap();
        let path = file.to_str().unwrap();
        let first = FileStore.identify(path, None).unwrap();
        assert!(first.key.starts_with("f-"), "{}", first.key);
        assert_eq!(first, FileStore.identify(path, None).unwrap());
        std::fs::write(&file, "two").unwrap();
        assert_ne!(first.key, FileStore.identify(path, None).unwrap().key);
    }

    #[test]
    fn the_file_is_the_tar_and_a_missing_one_says_which() {
        let dir = TempDir::new("file-store").unwrap();
        let file = dir.path().join("b.tar");
        std::fs::write(&file, "x").unwrap();
        let id = FileStore.identify(file.to_str().unwrap(), None).unwrap();
        assert_eq!(FileStore.fetch(&id, dir.path()).unwrap(), file);
        let err = FileStore.identify("/no/such.tar", None).unwrap_err();
        assert!(format!("{err:#}").contains("/no/such.tar"), "{err:#}");
    }
}
