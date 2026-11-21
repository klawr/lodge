//! The cache of readied bundle directories, under `~/.cache/lodge/bundles/`.
//!
//! An entry is a directory named by its key: the one a bundle store gave it
//! (`f-<file hash>`, `o-<image digest>`), or `p-<inputs fingerprint>` for a live
//! pack. Entries appear
//! atomically (staged, then renamed into place), are marked as used by their
//! modification time, and all but the [`KEEP`] most recently used are pruned.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::manifest::{MANIFEST_FILE, Manifest};
use crate::arch::Arch;
use crate::tmpfile::{TempDir, discard_dir};
use crate::tree::{self, Denied, Links};

/// How many cached bundles to keep (the most recently used).
pub const KEEP: usize = 4;

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
        tags.retain(|_, k| root.join(k).is_dir());
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
    load(&root.join(&key)).map(|_| key)
}

/// Where prepared bundles are cached: `~/.cache/lodge/bundles`.
pub fn root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("this system has no cache directory")?
        .join("lodge")
        .join("bundles"))
}

/// One cached bundle directory.
pub struct Entry {
    pub dir: PathBuf,
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
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let dir = e.path();
            let manifest = std::fs::read(dir.join(MANIFEST_FILE))
                .ok()
                .and_then(|b| Manifest::from_json(&b).ok());
            Entry {
                size: dir_size(&dir),
                modified: e.metadata().and_then(|m| m.modified()).ok(),
                manifest,
                dir,
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
        discard_dir(&e.dir);
    }
    // What the references named points at nothing now.
    std::fs::remove_file(root.join(TAGS_FILE)).ok();
    (all.len(), size)
}

/// Bytes on disk under `path`. Best effort: it is only shown, so whatever
/// could be measured before a failure is the answer.
fn dir_size(path: &Path) -> u64 {
    let mut size = 0;
    tree::walk(path, Links::Keep, Denied::Skip, &mut |e| {
        if !e.meta.is_dir() {
            size += e.meta.len();
        }
        Ok(())
    })
    .ok();
    size
}

/// The manifest of the entry in `dir`, marking it as recently used; `None` if
/// there is no valid bundle there.
pub(super) fn load(dir: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(dir.join(MANIFEST_FILE)).ok()?;
    let manifest = Manifest::from_json(&bytes).ok()?;
    touch(dir);
    Some(manifest)
}

/// A staging directory under `root`, on the same filesystem as the entries so
/// it can be moved into place with [`install`].
pub(super) fn staging(root: &Path, name: &str) -> Result<TempDir> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    Ok(TempDir::new_in(root, name)?)
}

/// Moves a finished staging directory into place. Atomic, so an entry is either
/// complete or absent. If a concurrent run got there first its copy is just as
/// good.
pub(super) fn install(staging: TempDir, dir: &Path) -> Result<PathBuf> {
    match std::fs::rename(staging.path(), dir) {
        Ok(()) => Ok(dir.to_path_buf()),
        Err(_) if dir.join(MANIFEST_FILE).exists() => Ok(dir.to_path_buf()),
        Err(e) => Err(e).with_context(|| format!("saving the bundle to {}", dir.display())),
    }
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "recency is only a pruning hint"
)]
fn touch(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
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
            discard_dir(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_from_a_registry_are_pruned_like_the_others() {
        let root = TempDir::new("prune-o").unwrap();
        for n in 0..KEEP + 2 {
            let dir = root.path().join(format!("o-{n}"));
            std::fs::create_dir(&dir).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
            touch(&dir);
        }
        let keep = root.path().join("o-0");
        prune(root.path(), &keep);
        let left = std::fs::read_dir(root.path()).unwrap().count();
        // The most recent ones stay, and so does the one in use, whatever its age.
        assert_eq!(left, KEEP + 1, "{left}");
        assert!(keep.exists());
    }

    #[test]
    fn clearing_the_cache_forgets_what_the_references_named() {
        let root = TempDir::new("clear-tags").unwrap();
        std::fs::create_dir(root.path().join("o-a")).unwrap();
        remember(root.path(), "oci://x/y:1", None, "o-a");
        assert!(root.path().join(TAGS_FILE).exists());
        clear(root.path());
        assert!(!root.path().join(TAGS_FILE).exists());
    }
}
