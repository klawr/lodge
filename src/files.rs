//! The text files lodge keeps for itself: `config.toml` and `client.toml`.
//!
//! Reading and replacing them is a dependency like the network or the terminal:
//! it sits behind [`Files`], the command layer hands in [`DiskFiles`], and a test
//! hands in files in memory, so the loading and editing of the config is
//! tested without a folder on disk and a failing disk is one line. It is not
//! [`crate::local::Local`], which is the machine's environment and programs: what
//! needs files of this kind does not need those, and the other way round.

use std::io;
use std::path::Path;

pub trait Files: Send + Sync {
    /// The text of the file at `path`, `None` when there is no such file.
    fn read(&self, path: &Path) -> io::Result<Option<String>>;

    /// Makes the file at `path` hold `text` (and its folder, if it is not there):
    /// all of it or, if that fails, what was there before.
    fn write(&self, path: &Path, text: &str) -> io::Result<()>;

    /// Whether there is a file at `path`.
    fn exists(&self, path: &Path) -> bool {
        self.read(path).is_ok_and(|text| text.is_some())
    }
}

/// The files of this machine.
pub struct DiskFiles;

impl Files for DiskFiles {
    fn read(&self, path: &Path) -> io::Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&self, path: &Path, text: &str) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Written beside it and moved over it, so a failure half way leaves the
        // old file whole.
        let mut staged = path.as_os_str().to_owned();
        staged.push(".tmp");
        let staged = std::path::PathBuf::from(staged);
        std::fs::write(&staged, text)?;
        std::fs::rename(&staged, path).inspect_err(|_| {
            std::fs::remove_file(&staged).ok();
        })
    }
}

#[cfg(test)]
pub use fake::{MemoryFiles, disk};

#[cfg(test)]
mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// The real files, for the tests that want a folder on disk anyway.
    pub fn disk() -> Arc<dyn Files> {
        Arc::new(DiskFiles)
    }

    /// Files in memory: what was written can be read back, and a disk that
    /// refuses to write is one call away.
    #[derive(Default)]
    pub struct MemoryFiles {
        files: Mutex<BTreeMap<PathBuf, String>>,
        refuse: Mutex<Option<String>>,
        writes: Mutex<usize>,
    }

    impl MemoryFiles {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn with(self: Arc<Self>, path: &str, text: &str) -> Arc<Self> {
            self.files
                .lock()
                .unwrap()
                .insert(PathBuf::from(path), text.to_string());
            self
        }

        /// Every write from now on fails with `why`.
        pub fn refusing(self: Arc<Self>, why: &str) -> Arc<Self> {
            *self.refuse.lock().unwrap() = Some(why.to_string());
            self
        }

        /// What the file at `path` holds now.
        pub fn text(&self, path: &str) -> Option<String> {
            self.files.lock().unwrap().get(Path::new(path)).cloned()
        }

        /// How many times something was written.
        pub fn writes(&self) -> usize {
            *self.writes.lock().unwrap()
        }
    }

    impl Files for MemoryFiles {
        fn read(&self, path: &Path) -> io::Result<Option<String>> {
            Ok(self.files.lock().unwrap().get(path).cloned())
        }

        fn write(&self, path: &Path, text: &str) -> io::Result<()> {
            if let Some(why) = self.refuse.lock().unwrap().clone() {
                return Err(io::Error::other(why));
            }
            *self.writes.lock().unwrap() += 1;
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), text.to_string());
            Ok(())
        }
    }

    #[test]
    fn disk_files_read_what_was_written_and_a_missing_file_is_none() {
        let dir = crate::tmpfile::TempDir::new("disk-files").unwrap();
        let path = dir.path().join("deeper/file.toml");
        let files = DiskFiles;
        assert_eq!(files.read(&path).unwrap(), None);
        assert!(!files.exists(&path));
        files.write(&path, "a = 1\n").unwrap();
        assert_eq!(files.read(&path).unwrap().as_deref(), Some("a = 1\n"));
        files.write(&path, "a = 2\n").unwrap();
        assert_eq!(files.read(&path).unwrap().as_deref(), Some("a = 2\n"));
        assert!(files.exists(&path));
        // Nothing staged is left beside it.
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        // Anything but "not there" is an error.
        assert!(files.read(dir.path()).is_err());
    }

    #[test]
    fn memory_files_behave_like_the_disk_ones_and_can_refuse() {
        let files = MemoryFiles::new().with("/a/b.toml", "x\n");
        assert_eq!(
            files.read(Path::new("/a/b.toml")).unwrap().as_deref(),
            Some("x\n")
        );
        assert!(files.read(Path::new("/a/none")).unwrap().is_none());
        files.write(Path::new("/a/c.toml"), "y\n").unwrap();
        assert_eq!(files.text("/a/c.toml").as_deref(), Some("y\n"));
        let files = files.refusing("disk full");
        let e = files.write(Path::new("/a/d.toml"), "z").unwrap_err();
        assert!(e.to_string().contains("disk full"));
        assert_eq!(files.writes(), 1);
        assert!(files.text("/a/d.toml").is_none());
    }
}
