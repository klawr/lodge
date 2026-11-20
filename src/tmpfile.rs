//! Self-deleting temp files, for staging bytes locally before a `docker cp`.
//!
//! The temp directory is shared with every other user, so names are never
//! trusted: each file or directory is created new (never opened if something
//! is already there, symlink or not) and private to this user.

use anyhow::Result;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many names to try before giving up on one that is free.
const ATTEMPTS: u32 = 100;

/// Creates something new at `<parent>/lodge-<pid>-<n>-<name>` with `create`,
/// moving on to the next `n` when the name is taken.
fn create_new(
    parent: &Path,
    name: &str,
    create: impl Fn(&Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    create_new_from(
        parent,
        name,
        &mut || COUNTER.fetch_add(1, Ordering::Relaxed),
        create,
    )
}

/// [`create_new`] with the `n`s drawn from `next`.
fn create_new_from(
    parent: &Path,
    name: &str,
    next: &mut dyn FnMut() -> u64,
    create: impl Fn(&Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    for _ in 0..ATTEMPTS {
        let n = next();
        let path = parent.join(format!("lodge-{}-{n}-{name}", std::process::id()));
        match create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free name for {name} in {}", parent.display()),
    ))
}

/// Creates the file `p`, owner-only, failing if anything (a symlink included)
/// is already there.
fn create_private_file(p: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)
        .map(drop)
}

/// A temp file that deletes itself on drop, so an error that short-circuits an
/// injection doesn't leave `lodge-*` files behind in the system temp dir.
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Writes `contents` to a new file `lodge-<pid>-<n>-<name>` in the system
    /// temp dir, readable by this user only.
    pub fn write(name: &str, contents: &[u8]) -> Result<Self> {
        Self::write_in(&std::env::temp_dir(), name, contents)
    }

    /// Like [`write`](Self::write) but in `dir`.
    pub fn write_in(dir: &Path, name: &str, contents: &[u8]) -> Result<Self> {
        Self::fill(create_new(dir, name, create_private_file)?, contents)
    }

    /// Takes ownership of the new, empty file at `path` and writes `contents`.
    fn fill(path: PathBuf, contents: &[u8]) -> Result<Self> {
        // Owned from here on, so a failed write still removes the file.
        let file = Self { path };
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file.path)?
            .write_all(contents)?;
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        discard_file(&self.path);
    }
}

/// A scratch directory that removes itself, and everything in it, on drop, so an
/// error (or a test failing halfway) doesn't leave files behind.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A new directory `lodge-<pid>-<n>-<name>` in the system temp dir, private
    /// to this user.
    pub fn new(name: &str) -> std::io::Result<Self> {
        Self::new_in(&std::env::temp_dir(), name)
    }

    /// Like [`new`](Self::new) but inside `parent`, so the directory can later be
    /// renamed into place on the same filesystem.
    pub fn new_in(parent: &Path, name: &str) -> std::io::Result<Self> {
        let path = create_new(parent, name, |p| {
            std::fs::DirBuilder::new().mode(0o700).create(p)
        })?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        discard_dir(&self.path);
    }
}

/// Removes a scratch file. Best effort: a leftover temp file is not worth
/// failing the operation that is already finished with it.
#[allow(clippy::let_underscore_must_use, reason = "best-effort cleanup")]
pub fn discard_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Removes a scratch directory and everything in it. Best effort, like
/// [`discard_file`].
#[allow(clippy::let_underscore_must_use, reason = "best-effort cleanup")]
pub fn discard_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

#[cfg(test)]
mod tests {
    use super::{TempDir, TempFile, create_new_from, create_private_file};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn files_and_folders_are_private_and_never_reuse_what_is_there() {
        let dir = TempDir::new("private").unwrap();
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Names planted in advance (here: symlinks to a file of someone
        // else's) are skipped, not written through. The counter is the test's
        // own, so the planted names are exactly the ones tried first.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let name = |k: u64| format!("lodge-{}-{k}-x", std::process::id());
        for k in 0..3 {
            std::os::unix::fs::symlink(&victim, dir.path().join(name(k))).unwrap();
        }
        let mut n = 0;
        let mut next = || {
            n += 1;
            n - 1
        };
        let path = create_new_from(dir.path(), "x", &mut next, create_private_file).unwrap();
        assert_eq!(
            path,
            dir.path().join(name(3)),
            "the planted names were tried"
        );
        let f = TempFile::fill(path, b"new").unwrap();
        assert!(!f.path().is_symlink());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(
            std::fs::metadata(f.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn same_name_gets_distinct_paths_and_each_cleans_up_itself() {
        let a = TempFile::write("same", b"a").unwrap();
        let b = TempFile::write("same", b"b").unwrap();
        assert_ne!(a.path(), b.path());
        let a_path = a.path().to_path_buf();
        drop(a);
        assert!(!a_path.exists(), "dropped file is removed");
        assert_eq!(
            std::fs::read(b.path()).unwrap(),
            b"b",
            "the other is untouched"
        );
    }
}
