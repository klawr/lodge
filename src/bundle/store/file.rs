//! A bundle that is a file on this machine.

use super::{BundleSink, BundleStore, Identity, Replace};
use crate::arch::Arch;
use crate::bundle::hash::hash_path;
use anyhow::{Context, Result, bail};
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

/// Writes a bundle to a file path, which appears only when it is complete: it is
/// written beside the destination and renamed into place.
pub struct FileSink;

impl BundleSink for FileSink {
    fn form(&self) -> &'static str {
        "a file path"
    }

    fn handles(&self, reference: &str) -> bool {
        !reference.contains("://")
    }

    /// Beside the destination, where the tar can be renamed into place.
    fn staging(&self, reference: &str) -> Option<PathBuf> {
        let dir = Path::new(reference)
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        dir.is_dir().then(|| dir.to_path_buf())
    }

    fn put(&self, reference: &str, tar: &Path, _arch: Arch, replace: Replace) -> Result<String> {
        if replace == Replace::No && Path::new(reference).exists() {
            bail!("{reference} exists already");
        }
        // On the same filesystem this is the whole job, and atomic.
        if std::fs::rename(tar, reference).is_ok() {
            return Ok(reference.into());
        }
        // Elsewhere: written beside the destination and renamed, so the file
        // appears only when it is whole.
        let partial = PathBuf::from(format!("{reference}.partial"));
        let placed = std::fs::copy(tar, &partial)
            .with_context(|| format!("writing {}", partial.display()))
            .and_then(|_| {
                std::fs::rename(&partial, reference)
                    .with_context(|| format!("moving the bundle to {reference}"))
            });
        if placed.is_err() {
            crate::tmpfile::discard_file(&partial);
        }
        placed?;
        Ok(reference.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    #[test]
    fn a_bundle_is_written_whole_and_replaces_what_was_there() {
        let dir = TempDir::new("file-sink").unwrap();
        let tar = dir.path().join("in.tar");
        let out = dir.path().join("out.tar");
        std::fs::write(&tar, "new").unwrap();
        std::fs::write(&out, "old").unwrap();
        let said = FileSink
            .put(out.to_str().unwrap(), &tar, Arch::X86_64, Replace::Yes)
            .unwrap();
        assert_eq!(said, out.to_str().unwrap());
        assert_eq!(std::fs::read(&out).unwrap(), b"new");
        // The tar was moved into place, and nothing else is left beside it.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("out.tar")]);
    }

    #[test]
    fn the_tar_is_staged_beside_the_destination_and_renamed_into_place() {
        let dir = TempDir::new("file-sink").unwrap();
        let out = dir.path().join("out.tar");
        let reference = out.to_str().unwrap();
        // A folder that exists is where to build; one that does not is no place.
        assert_eq!(FileSink.staging(reference).as_deref(), Some(dir.path()));
        assert_eq!(FileSink.staging("/no/such/dir/out.tar"), None);
        assert_eq!(FileSink.staging("out.tar").as_deref(), Some(Path::new(".")));
        // Built there, placing it moves it: the staged file is gone afterwards.
        let staged = TempDir::new_in(dir.path(), "stage").unwrap();
        let tar = staged.path().join("bundle.tar");
        std::fs::write(&tar, "whole").unwrap();
        FileSink
            .put(reference, &tar, Arch::X86_64, Replace::Yes)
            .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"whole");
        assert!(!tar.exists());
    }

    #[test]
    fn a_destination_that_cannot_be_written_says_so_and_leaves_nothing() {
        let dir = TempDir::new("file-sink").unwrap();
        let tar = dir.path().join("in.tar");
        std::fs::write(&tar, "x").unwrap();
        let out = dir.path().join("no-such-dir/out.tar");
        let err = FileSink
            .put(out.to_str().unwrap(), &tar, Arch::X86_64, Replace::Yes)
            .unwrap_err();
        assert!(format!("{err:#}").contains("out.tar"), "{err:#}");
    }

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

    #[test]
    fn a_file_that_exists_is_kept_when_replacing_is_refused() {
        let dir = TempDir::new("file-sink").unwrap();
        let tar = dir.path().join("in.tar");
        let out = dir.path().join("out.tar");
        std::fs::write(&tar, b"new").unwrap();
        std::fs::write(&out, b"old").unwrap();
        let err = FileSink
            .put(out.to_str().unwrap(), &tar, Arch::X86_64, Replace::No)
            .unwrap_err();
        assert!(err.to_string().contains("exists already"), "{err}");
        assert_eq!(std::fs::read(&out).unwrap(), b"old");
        assert!(tar.exists());
        // Where nothing is, it is written.
        let fresh = dir.path().join("fresh.tar");
        FileSink
            .put(fresh.to_str().unwrap(), &tar, Arch::X86_64, Replace::No)
            .unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
    }
}
