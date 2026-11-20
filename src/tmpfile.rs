//! Self-deleting temp files, for staging bytes locally before a `docker cp`.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp file that deletes itself on drop, so an error that short-circuits an
/// injection doesn't leave `lodge-*` files behind in the system temp dir.
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Writes `contents` to `lodge-<pid>-<n>-<name>` in the system temp dir. The
    /// pid keeps concurrent `ldg` runs from clobbering each other's staging
    /// files, and the counter does the same for two files in one process that
    /// share a name.
    pub fn write(name: &str, contents: &[u8]) -> Result<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("lodge-{}-{n}-{name}", std::process::id()));
        std::fs::write(&path, contents)?;
        Ok(Self { path })
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
    pub fn new(name: &str) -> std::io::Result<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("lodge-{}-{n}-{name}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Like [`new`](Self::new) but inside `parent`, so the directory can later be
    /// renamed into place on the same filesystem.
    pub fn new_in(parent: &Path, name: &str) -> std::io::Result<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("lodge-{}-{n}-{name}", std::process::id()));
        std::fs::create_dir_all(&path)?;
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
    use super::TempFile;

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
