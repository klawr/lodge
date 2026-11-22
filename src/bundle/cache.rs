//! The cache of readied bundles, under `~/.cache/lodge/bundles/`.
//!
//! An entry is a bundle's tar, verified, named by its key: the one a bundle
//! store gave it (`f-<file hash>`, `o-<image digest>`), or `p-<inputs
//! fingerprint>` for a live pack, and `.tar`. It is kept as the tar and read in
//! place (see [`super::reader`]), never unpacked. Entries appear atomically
//! (staged, then renamed into place), are marked as used by their modification
//! time, and all but the [`KEEP`] most recently used are pruned.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::manifest::Manifest;
use super::reader::Bundle;
use crate::arch::Arch;
use crate::tmpfile::{TempDir, discard_dir, discard_file};

/// How many cached bundles to keep (the most recently used).
pub const KEEP: usize = 4;

/// The extension of an entry.
const EXTENSION: &str = "tar";

/// The name a bundle's tar has in a [`staging`] directory.
const STAGED: &str = "bundle.tar";

/// The file, beside the entries, that remembers which bundle a reference named
/// the last time its place could be asked.
const TAGS_FILE: &str = "tags.json";

type Tags = std::collections::BTreeMap<String, String>;

fn tag_key(reference: &str, arch: Option<Arch>) -> String {
    format!(
        "{reference}\n{}",
        arch.map(|a| a.to_string()).unwrap_or_default()
    )
}

fn read_tags(root: &Path) -> Tags {
    std::fs::read(root.join(TAGS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Notes that `reference` (for `arch`) named the cached bundle `key`, so that a
/// place which cannot be reached later can still be answered from the cache. Only
/// a convenience: a failure to write it is a trace line, not an error.
pub(super) fn remember(root: &Path, reference: &str, arch: Option<Arch>, key: &str) {
    let write = || -> Result<()> {
        let mut tags = read_tags(root);
        // What was pruned from the cache cannot be answered from it.
        tags.retain(|_, key| entry_path(root, key).is_file());
        tags.insert(tag_key(reference, arch), key.to_string());
        let partial = root.join(format!("{TAGS_FILE}.partial"));
        std::fs::write(&partial, serde_json::to_vec_pretty(&tags)?)?;
        std::fs::rename(&partial, root.join(TAGS_FILE))?;
        Ok(())
    };
    if let Err(e) = write() {
        crate::output::trace_line(&format!("not remembering {reference}: {e:#}"));
    }
}

/// The key of the cached bundle `reference` (for `arch`) named last time, if that
/// bundle is still in the cache.
pub(super) fn recall(root: &Path, reference: &str, arch: Option<Arch>) -> Option<String> {
    let key = read_tags(root).remove(&tag_key(reference, arch))?;
    load(root, &key).map(|_| key)
}

/// Where prepared bundles are cached: `~/.cache/lodge/bundles`.
pub fn root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("this system has no cache directory")?
        .join("lodge")
        .join("bundles"))
}

/// Where the entry `key` is.
fn entry_path(root: &Path, key: &str) -> PathBuf {
    root.join(format!("{key}.{EXTENSION}"))
}

/// One cached bundle.
pub struct Entry {
    pub file: PathBuf,
    /// The bundle's manifest, if the entry is complete.
    pub manifest: Option<Manifest>,
    /// Bytes on disk.
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// The cached bundles under `root`, most recently used first.
pub fn entries(root: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = read
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == EXTENSION))
        .map(|e| {
            let file = e.path();
            let meta = e.metadata().ok();
            Entry {
                manifest: Bundle::open(&file).ok().map(|b| b.manifest().clone()),
                size: meta.as_ref().map_or(0, std::fs::Metadata::len),
                modified: meta.and_then(|m| m.modified().ok()),
                file,
            }
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.modified));
    out
}

/// Removes every cached bundle under `root`; returns how many and their size.
pub fn clear(root: &Path) -> (usize, u64) {
    let all = entries(root);
    let size = all.iter().map(|e| e.size).sum();
    for e in &all {
        discard_file(&e.file);
    }
    // What the references named points at nothing now.
    std::fs::remove_file(root.join(TAGS_FILE)).ok();
    (all.len(), size)
}

/// The cached bundle `key`, marking it as recently used; `None` if there is no
/// usable one. The content is not checked again: it was when it was cached.
pub(super) fn load(root: &Path, key: &str) -> Option<Bundle> {
    let file = entry_path(root, key);
    let bundle = Bundle::open(&file).ok()?;
    touch(&file);
    Some(bundle)
}

/// A staging directory under `root`, on the same filesystem as the entries so
/// what is built in it can be moved into place with [`install`].
pub(super) fn staging(root: &Path, name: &str) -> Result<TempDir> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    Ok(TempDir::new_in(root, name)?)
}

/// Where the bundle's tar goes in `staging`, for [`install`] to find.
pub(super) fn staged(staging: &TempDir) -> PathBuf {
    staging.path().join(STAGED)
}

/// Puts the tar at `tar` where [`install`] finds it: moved when it is already in
/// `staging` (a store that fetched it there), copied otherwise (a file the user
/// keeps). Returns where it is.
pub(super) fn stage_tar(staging: &TempDir, tar: &Path) -> Result<PathBuf> {
    let to = staged(staging);
    if tar.starts_with(staging.path()) {
        std::fs::rename(tar, &to)
    } else {
        std::fs::copy(tar, &to).map(|_| ())
    }
    .with_context(|| format!("keeping {}", tar.display()))?;
    Ok(to)
}

/// Moves the tar staged in `staging` into place as the entry `key`, and opens
/// it. Atomic, so an entry is either complete or absent. If a concurrent run got
/// there first its copy is just as good.
pub(super) fn install(root: &Path, staging: TempDir, key: &str) -> Result<Bundle> {
    let file = entry_path(root, key);
    if let Err(e) = std::fs::rename(staged(&staging), &file)
        && load(root, key).is_none()
    {
        return Err(e).with_context(|| format!("saving the bundle to {}", file.display()));
    }
    load(root, key)
        .with_context(|| format!("the bundle just saved to {} is unreadable", file.display()))
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "recency is only a pruning hint"
)]
fn touch(file: &Path) {
    if let Ok(handle) = std::fs::File::open(file) {
        let _ = handle.set_modified(SystemTime::now());
    }
}

/// Removes the least recently used entries beyond [`KEEP`], and staging
/// directories left behind by a crashed run. `keep` is never removed.
pub(super) fn prune(root: &Path, keep: &Path) {
    let Ok(read) = std::fs::read_dir(root) else {
        return;
    };
    let mut cached = Vec::new();
    for entry in read.filter_map(std::result::Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified = entry.metadata().and_then(|m| m.modified()).ok();
        if name.starts_with("f-") || name.starts_with("o-") || name.starts_with("p-") {
            cached.push((modified, entry.path()));
        } else if name.starts_with("lodge-")
            && modified
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 3600)
        {
            discard_dir(&entry.path());
        }
    }
    cached.sort_by_key(|entry| std::cmp::Reverse(entry.0)); // newest first
    for (_, path) in cached.into_iter().skip(KEEP) {
        if path != keep {
            // (Entries of an older layout were directories.)
            if path.is_dir() {
                discard_dir(&path);
            } else {
                discard_file(&path);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::bundle::testing::{bundle_dir, tar_of};

    /// An entry `key` holding the bundle of a small directory.
    fn put(root: &Path, key: &str) {
        let (dir, _) = bundle_dir();
        let (_keep, tar) = tar_of(dir.path(), "cache-entry");
        std::fs::copy(tar, entry_path(root, key)).unwrap();
    }
    #[test]
    fn bundles_from_a_registry_are_pruned_like_the_others() {
        let root = TempDir::new("prune-o").unwrap();
        for n in 0..KEEP + 2 {
            put(root.path(), &format!("o-{n}"));
            std::thread::sleep(std::time::Duration::from_millis(15));
            touch(&entry_path(root.path(), &format!("o-{n}")));
        }
        let keep = entry_path(root.path(), "o-0");
        prune(root.path(), &keep);
        let left = std::fs::read_dir(root.path()).unwrap().count();
        // The most recent ones stay, and so does the one in use, whatever its age.
        assert_eq!(left, KEEP + 1, "{left}");
        assert!(keep.exists());
    }
    #[test]
    fn clearing_the_cache_forgets_what_the_references_named() {
        let root = TempDir::new("clear-tags").unwrap();
        put(root.path(), "o-a");
        remember(root.path(), "oci://x/y:1", None, "o-a");
        assert!(root.path().join(TAGS_FILE).exists());
        assert_eq!(clear(root.path()).0, 1);
        assert!(!root.path().join(TAGS_FILE).exists());
        assert!(entries(root.path()).is_empty());
    }
    #[test]
    fn an_entry_is_installed_whole_and_listed_with_its_manifest() {
        let root = TempDir::new("install").unwrap();
        let staging = staging(root.path(), "t").unwrap();
        let (dir, manifest) = bundle_dir();
        let (_keep, tar) = tar_of(dir.path(), "to-stage");
        let staged_at = stage_tar(&staging, &tar).unwrap();
        assert!(tar.exists(), "a file the user keeps is copied, not moved");
        assert_eq!(staged_at, staged(&staging));

        let bundle = install(root.path(), staging, "f-1").unwrap();
        assert_eq!(bundle.manifest(), &manifest);
        let listed = entries(root.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].manifest.as_ref(), Some(&manifest));
        assert_eq!(load(root.path(), "f-1").unwrap().manifest(), &manifest);
        assert!(load(root.path(), "f-2").is_none());
    }
    #[test]
    fn a_tar_a_store_fetched_into_staging_is_moved_not_copied() {
        let root = TempDir::new("stage-move").unwrap();
        let staging = staging(root.path(), "t").unwrap();
        let (dir, _) = bundle_dir();
        let (_keep, tar) = tar_of(dir.path(), "fetched");
        let fetched = staging.path().join("sha256-abc");
        std::fs::copy(&tar, &fetched).unwrap();
        stage_tar(&staging, &fetched).unwrap();
        assert!(!fetched.exists());
    }
}
