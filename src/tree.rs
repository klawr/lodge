//! Walking a file or directory tree on this machine in a stable order.
//!
//! Everything that looks at a whole tree (hashing a bundle's contents,
//! fingerprinting a config tree, hashing `.devcontainer/`, copying a config into
//! a bundle, sizing the cache) goes through [`walk`], so they all agree on the
//! order, on symlinks and on what an unreadable directory means.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How symlinks are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Links {
    /// Reported as symlinks, never followed: what a tree *contains*.
    Keep,
    /// Followed: what a program reading the tree would *see*.
    Follow,
}

/// What a directory that can't be listed means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// An error.
    Fail,
    /// Left out, with everything below it (permission denied only).
    Skip,
}

/// One entry of the tree.
pub struct Entry<'a> {
    pub path: &'a Path,
    /// The path relative to the root, `/`-separated; empty for the root itself.
    pub relative: &'a str,
    /// Metadata, of the link itself under [`Links::Keep`].
    pub meta: &'a Metadata,
}

/// Visits `root` and, if it is a directory, everything below it: depth first,
/// a directory before its contents, the entries of each directory in name
/// order. Under [`Links::Follow`] a link back to a directory being walked (a
/// loop) is visited but not entered again.
pub fn walk(
    root: &Path,
    links: Links,
    denied: Denied,
    visit: &mut dyn FnMut(&Entry) -> Result<()>,
) -> Result<()> {
    Walker {
        links,
        denied,
        open: Vec::new(),
        visit,
    }
    .visit(root, String::new())
}

struct Walker<'v> {
    links: Links,
    denied: Denied,
    /// (device, inode) of the directories being walked, root first.
    open: Vec<(u64, u64)>,
    visit: &'v mut dyn FnMut(&Entry) -> Result<()>,
}

impl Walker<'_> {
    fn visit(&mut self, path: &Path, relative: String) -> Result<()> {
        let meta = match self.links {
            Links::Keep => std::fs::symlink_metadata(path),
            Links::Follow => std::fs::metadata(path),
        }
        .with_context(|| format!("reading {}", path.display()))?;
        (self.visit)(&Entry {
            path,
            relative: &relative,
            meta: &meta,
        })?;
        let id = (meta.dev(), meta.ino());
        if !meta.is_dir() || self.open.contains(&id) {
            return Ok(());
        }
        let Some(children) = self.children(path)? else {
            return Ok(());
        };
        self.open.push(id);
        for (name, child) in children {
            let name = name.to_string_lossy();
            let relative = if relative.is_empty() {
                name.into_owned()
            } else {
                format!("{relative}/{name}")
            };
            self.visit(&child, relative)?;
        }
        self.open.pop();
        Ok(())
    }

    /// The entries of the directory `path` in name order; `None` if it may be
    /// skipped because it can't be listed.
    fn children(&self, path: &Path) -> Result<Option<Vec<(OsString, PathBuf)>>> {
        let read = match std::fs::read_dir(path) {
            Ok(read) => read,
            Err(e)
                if self.denied == Denied::Skip
                    && e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut children: Vec<(OsString, PathBuf)> = read
            .map(|e| e.map(|e| (e.file_name(), e.path())))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading {}", path.display()))?;
        children.sort();
        Ok(Some(children))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn visited(root: &Path, links: Links) -> Vec<String> {
        let mut seen = Vec::new();
        walk(root, links, Denied::Fail, &mut |e| {
            let kind = if e.meta.is_symlink() {
                "L"
            } else if e.meta.is_dir() {
                "D"
            } else {
                "F"
            };
            seen.push(format!("{kind} {}", e.relative));
            Ok(())
        })
        .unwrap();
        seen
    }

    #[test]
    fn directories_come_before_their_contents_in_name_order() {
        let t = TempDir::new("walk").unwrap();
        let p = t.path();
        std::fs::create_dir_all(p.join("b/c")).unwrap();
        std::fs::write(p.join("b/c/x"), "").unwrap();
        std::fs::write(p.join("a"), "").unwrap();
        std::os::unix::fs::symlink("b", p.join("z")).unwrap();
        assert_eq!(
            visited(p, Links::Keep),
            ["D ", "F a", "D b", "D b/c", "F b/c/x", "L z"]
        );
        // Followed, the link is the directory it points at.
        assert_eq!(
            visited(p, Links::Follow),
            [
                "D ", "F a", "D b", "D b/c", "F b/c/x", "D z", "D z/c", "F z/c/x"
            ]
        );
    }

    #[test]
    fn a_link_loop_is_entered_once() {
        let t = TempDir::new("walk-loop").unwrap();
        let p = t.path();
        std::fs::create_dir_all(p.join("a")).unwrap();
        std::fs::write(p.join("a/f"), "").unwrap();
        std::os::unix::fs::symlink("..", p.join("a/up")).unwrap();
        assert_eq!(visited(p, Links::Follow), ["D ", "D a", "F a/f", "D a/up"]);
    }

    #[test]
    fn a_single_file_is_its_own_tree() {
        let t = TempDir::new("walk-file").unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, "").unwrap();
        assert_eq!(visited(&f, Links::Keep), ["F "]);
        assert!(
            walk(
                &t.path().join("missing"),
                Links::Keep,
                Denied::Skip,
                &mut |_| Ok(())
            )
            .is_err()
        );
    }
}
