//! A snapshot: files, directories and links as the Linux target will see them.
//!
//! What lodge hashes and sends into a container is a *tree with Unix modes and
//! links*. Where that tree is held is another matter: a directory on this
//! machine, a bundle's tar, bytes in memory. A [`Snapshot`] is the one
//! description of it that hashing ([`crate::bundle::hash`]) and copying
//! ([`crate::runtime::tarball`]) are written against, so neither asks the
//! filesystem of the machine lodge runs on what a mode is: a filesystem that has
//! no such thing never has to be asked.
//!
//! [`DiskSnapshot`] is the only implementation that reads `std::fs` metadata,
//! and so the only place that knows how this machine's files map onto modes.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::tree::{self, Denied, Links};

/// What a [`Node`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// A symbolic link and where it points, as written (`/`-separated, lossily
    /// UTF-8: a target that is not text is not kept exactly).
    Link(String),
}

/// One entry of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The path from the root, `/`-separated; empty for the root itself.
    pub relative: String,
    pub kind: Kind,
    /// Permission bits (`0o755`), without the file type.
    pub mode: u32,
    /// Seconds since the Unix epoch.
    pub mtime: u64,
    /// Bytes of content: what [`Snapshot::open`] yields, 0 for what has none.
    pub size: u64,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// A node with the same facts under another name.
    pub fn renamed(&self, relative: impl Into<String>) -> Self {
        Self {
            relative: relative.into(),
            ..self.clone()
        }
    }
}

/// A tree of files that can be walked and read, wherever it is held.
pub trait Snapshot {
    /// The root: the directory, or the one file, the snapshot is of.
    fn root(&self) -> Result<Node>;

    /// Visits the root and, for a directory, everything below it: depth first, a
    /// directory before its contents, the entries of each directory in name
    /// order. Walking again visits the same things in the same order.
    fn walk(&self, visit: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()>;

    /// The content of the file at `relative` (a [`Node::relative`]; empty for a
    /// snapshot that is one file): exactly [`Node::size`] bytes.
    fn open(&self, relative: &str) -> Result<Box<dyn Read + '_>>;
}

/// A tree on this machine's filesystem.
pub struct DiskSnapshot {
    root: PathBuf,
}

impl DiskSnapshot {
    /// The tree at `path`. A link at `path` itself is a link, as it is for every
    /// entry below it: what a tree *contains*.
    pub fn at(path: &Path) -> Self {
        Self { root: path.into() }
    }

    /// The tree `path` leads to: a link at `path` itself (or on the way to it) is
    /// followed, so what is copied is what it points at and never a link to a
    /// path on this machine.
    pub fn resolved(path: &Path) -> Result<Self> {
        let root =
            std::fs::canonicalize(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Self { root })
    }

    fn path_of(&self, relative: &str) -> PathBuf {
        // `join("")` would make `root/`, which a single file is not under.
        if relative.is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        }
    }

    fn node(relative: &str, path: &Path, meta: &std::fs::Metadata) -> Result<Node> {
        let kind = if meta.is_symlink() {
            let target = std::fs::read_link(path)
                .with_context(|| format!("reading the link {}", path.display()))?;
            Kind::Link(target.to_string_lossy().into_owned())
        } else if meta.is_dir() {
            Kind::Dir
        } else if meta.is_file() {
            Kind::File
        } else {
            bail!(
                "{} is neither a file, a directory nor a link, so it can't be copied",
                path.display()
            )
        };
        Ok(Node {
            relative: relative.into(),
            kind,
            mode: meta.permissions().mode() & 0o7777,
            mtime: u64::try_from(meta.mtime()).unwrap_or(0),
            size: if meta.is_file() { meta.len() } else { 0 },
        })
    }
}

impl Snapshot for DiskSnapshot {
    fn root(&self) -> Result<Node> {
        let meta = std::fs::symlink_metadata(&self.root)
            .with_context(|| format!("reading {}", self.root.display()))?;
        Self::node("", &self.root, &meta)
    }

    fn walk(&self, visit: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()> {
        tree::walk(&self.root, Links::Keep, Denied::Fail, &mut |e| {
            visit(&Self::node(e.relative, e.path, e.meta)?)
        })
    }

    fn open(&self, relative: &str) -> Result<Box<dyn Read + '_>> {
        let path = self.path_of(relative);
        let file =
            std::fs::File::open(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Box::new(file))
    }
}

/// One file held in memory: for something lodge makes up (a script it writes)
/// rather than copies.
pub struct InMemoryFile {
    contents: Vec<u8>,
    mode: u32,
}

impl InMemoryFile {
    pub fn new(contents: &[u8], mode: u32) -> Self {
        Self {
            contents: contents.to_vec(),
            mode,
        }
    }
}

impl Snapshot for InMemoryFile {
    fn root(&self) -> Result<Node> {
        Ok(Node {
            relative: String::new(),
            kind: Kind::File,
            mode: self.mode,
            mtime: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            size: self.contents.len() as u64,
        })
    }

    fn walk(&self, visit: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()> {
        visit(&self.root()?)
    }

    fn open(&self, relative: &str) -> Result<Box<dyn Read + '_>> {
        if !relative.is_empty() {
            bail!("a single file has nothing at {relative}");
        }
        Ok(Box::new(self.contents.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn walked(snapshot: &dyn Snapshot) -> Vec<(String, Kind, u32)> {
        let mut seen = Vec::new();
        snapshot
            .walk(&mut |n| {
                seen.push((n.relative.clone(), n.kind.clone(), n.mode & 0o777));
                Ok(())
            })
            .unwrap();
        seen
    }

    #[test]
    fn a_directory_is_walked_in_name_order_with_its_modes_and_links() {
        let dir = TempDir::new("snap-dir").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/run"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("sub/run"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(dir.path().join("a"), "A").unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("link")).unwrap();

        let snapshot = DiskSnapshot::at(dir.path());
        let seen = walked(&snapshot);
        let names: Vec<&str> = seen.iter().map(|n| n.0.as_str()).collect();
        assert_eq!(names, ["", "a", "link", "sub", "sub/run"]);
        assert_eq!(seen[2].1, Kind::Link("a".into()));
        assert_eq!(seen[4].2, 0o755);
        assert_eq!(snapshot.root().unwrap().kind, Kind::Dir);

        let mut text = String::new();
        snapshot
            .open("sub/run")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "#!/bin/sh\n");
    }

    #[test]
    fn a_file_is_a_snapshot_of_its_own_and_is_opened_by_the_empty_name() {
        let dir = TempDir::new("snap-file").unwrap();
        std::fs::write(dir.path().join("f"), "hello").unwrap();
        let snapshot = DiskSnapshot::at(&dir.path().join("f"));
        assert_eq!(snapshot.root().unwrap().size, 5);
        let mut text = String::new();
        snapshot
            .open("")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");
    }

    #[test]
    fn a_link_is_followed_only_when_asked_to_resolve_it() {
        let dir = TempDir::new("snap-link").unwrap();
        std::fs::write(dir.path().join("real"), "x").unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("l")).unwrap();
        let link = dir.path().join("l");
        assert!(matches!(
            DiskSnapshot::at(&link).root().unwrap().kind,
            Kind::Link(_)
        ));
        assert_eq!(
            DiskSnapshot::resolved(&link).unwrap().root().unwrap().kind,
            Kind::File
        );
        assert!(DiskSnapshot::resolved(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn a_special_file_is_refused() {
        let dir = TempDir::new("snap-fifo").unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status();
        if made.is_ok_and(|s| s.success()) {
            let err = DiskSnapshot::at(dir.path())
                .walk(&mut |_| Ok(()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("neither a file"), "{err}");
        }
    }

    #[test]
    fn a_file_in_memory_has_the_mode_it_is_given() {
        let file = InMemoryFile::new(b"#!/bin/sh\n", 0o755);
        let root = file.root().unwrap();
        assert_eq!((root.kind, root.mode, root.size), (Kind::File, 0o755, 10));
        assert_eq!(walked(&file).len(), 1);
        let mut bytes = Vec::new();
        file.open("").unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"#!/bin/sh\n");
        assert!(file.open("x").is_err());
    }
}
