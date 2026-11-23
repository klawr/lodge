//! The text files lodge keeps for itself: `config.toml` and `client.toml`, and the
//! history of places a session was opened in.
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

    /// As [`write`](Files::write), and the file is for this user alone, even when
    /// a more open one was there before: for what names project folders and hosts.
    fn write_private(&self, path: &Path, text: &str) -> io::Result<()>;

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
        stage(path, text, false)
    }

    fn write_private(&self, path: &Path, text: &str) -> io::Result<()> {
        stage(path, text, true)
    }
}

/// Writes `text` beside `path` and moves it over `path`, so a failure half way
/// leaves the old file whole. With `private`, the staged file is made for this
/// user alone, and comes out of the move with those rights whatever was there.
fn stage(path: &Path, text: &str, private: bool) -> io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut staged = path.as_os_str().to_owned();
    staged.push(".tmp");
    let staged = std::path::PathBuf::from(staged);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    if private {
        crate::platform::private_file(&mut opts);
    }
    opts.open(&staged)?.write_all(text.as_bytes())?;
    std::fs::rename(&staged, path).inspect_err(|_| {
        std::fs::remove_file(&staged).ok();
    })
}

#[cfg(test)]
pub use fake::{MemoryFiles, disk};

#[cfg(test)]
mod fake {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
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
        private: Mutex<BTreeSet<PathBuf>>,
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

        /// Whether the file at `path` was last written for this user alone.
        pub fn is_private(&self, path: &str) -> bool {
            self.private.lock().unwrap().contains(Path::new(path))
        }

        /// How many times something was written.
        pub fn writes(&self) -> usize {
            *self.writes.lock().unwrap()
        }
    }

    impl MemoryFiles {
        fn put(&self, path: &Path, text: &str, private: bool) -> io::Result<()> {
            if let Some(why) = self.refuse.lock().unwrap().clone() {
                return Err(io::Error::other(why));
            }
            *self.writes.lock().unwrap() += 1;
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), text.to_string());
            let mut kept = self.private.lock().unwrap();
            if private {
                kept.insert(path.to_path_buf());
            } else {
                kept.remove(path);
            }
            Ok(())
        }
    }

    impl Files for MemoryFiles {
        fn read(&self, path: &Path) -> io::Result<Option<String>> {
            Ok(self.files.lock().unwrap().get(path).cloned())
        }

        fn write(&self, path: &Path, text: &str) -> io::Result<()> {
            self.put(path, text, false)
        }

        fn write_private(&self, path: &Path, text: &str) -> io::Result<()> {
            self.put(path, text, true)
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

    #[cfg(unix)]
    #[test]
    fn a_private_write_is_for_this_user_even_over_an_open_file() {
        let dir = crate::tmpfile::TempDir::new("disk-private").unwrap();
        let path = dir.path().join("history.json");
        std::fs::write(&path, "[]").unwrap();
        crate::platform::set_mode(&path, 0o644).unwrap();
        DiskFiles.write_private(&path, "[1]").unwrap();
        let mode = crate::platform::mode(&std::fs::metadata(&path).unwrap());
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert_eq!(DiskFiles.read(&path).unwrap().as_deref(), Some("[1]"));
    }
}
