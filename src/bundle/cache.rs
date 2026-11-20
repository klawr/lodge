//! The cache of readied bundle directories, under `~/.cache/lodge/bundles/`.
//!
//! An entry is a directory named by its key (`f-<file hash>` for an extracted
//! bundle file, `p-<inputs fingerprint>` for a live pack). Entries appear
//! atomically (staged, then renamed into place), are marked as used by their
//! modification time, and all but the [`KEEP`] most recently used are pruned.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::manifest::{MANIFEST_FILE, Manifest};
use crate::tmpfile::{TempDir, discard_dir};
use crate::tree::{self, Denied, Links};

/// How many cached bundles to keep (the most recently used).
pub const KEEP: usize = 4;

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
        if name.starts_with("f-") || name.starts_with("p-") {
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
