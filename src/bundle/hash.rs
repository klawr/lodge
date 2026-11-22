//! A content hash for a file or a whole directory tree.
//!
//! Deterministic: entries are visited in name order, and the hash covers each
//! entry's relative path, kind, the executable bit and the content (a symlink
//! contributes its target). Two trees hash the same exactly when they would
//! install the same, wherever they are held: it is defined over a [`Snapshot`],
//! so a bundle in a tar hashes as the directory it was packed from did.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

use crate::fingerprint::Fingerprint;
use crate::snapshot::{DiskSnapshot, Kind, Snapshot};

/// The hash of the tree at `path` on this machine.
pub fn hash_path(path: &Path) -> Result<String> {
    hash(&DiskSnapshot::at(path))
}

/// The hash of `snapshot`.
pub fn hash(snapshot: &dyn Snapshot) -> Result<String> {
    let mut h = Fingerprint::new();
    snapshot.walk(&mut |node| {
        // The root is "", its children "/name", theirs "/name/child".
        let label = if node.relative.is_empty() {
            String::new()
        } else {
            format!("/{}", node.relative)
        };
        match &node.kind {
            Kind::Link(target) => {
                h.field(b"L").field(label.as_bytes());
                h.field(target.as_bytes());
            }
            Kind::Dir => {
                h.field(b"D").field(label.as_bytes());
            }
            Kind::File => {
                h.field(b"F").field(label.as_bytes());
                h.put(&[u8::from(node.mode & 0o111 != 0)]);
                feed(snapshot.open(&node.relative)?, &mut h)
                    .with_context(|| format!("reading {}", node.relative))?;
            }
        }
        Ok(())
    })?;
    Ok(h.hex())
}

/// Streams a file through the hash, so a large binary is never held in memory.
fn feed(mut file: impl Read, h: &mut Fingerprint) -> std::io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        h.put(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new("tree").unwrap();
        for (rel, contents) in files {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contents).unwrap();
        }
        dir
    }

    #[test]
    fn the_same_tree_hashes_the_same_wherever_it_lives() {
        let a = tree(&[("init.lua", "x"), ("lua/p.lua", "y")]);
        let b = tree(&[("lua/p.lua", "y"), ("init.lua", "x")]);
        assert_eq!(hash_path(a.path()).unwrap(), hash_path(b.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn any_change_to_content_name_or_mode_changes_the_hash() {
        let base = tree(&[("a", "1")]);
        let h = hash_path(base.path()).unwrap();
        assert_ne!(h, hash_path(tree(&[("a", "2")]).path()).unwrap(), "content");
        assert_ne!(h, hash_path(tree(&[("b", "1")]).path()).unwrap(), "name");
        crate::platform::set_mode(base.path().join("a"), 0o755).unwrap();
        assert_ne!(h, hash_path(base.path()).unwrap(), "executable bit");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_counts_by_its_target_not_what_it_points_at() {
        let a = TempDir::new("link-a").unwrap();
        crate::platform::symlink("one", a.path().join("l")).unwrap();
        let b = TempDir::new("link-b").unwrap();
        crate::platform::symlink("two", b.path().join("l")).unwrap();
        assert_ne!(hash_path(a.path()).unwrap(), hash_path(b.path()).unwrap());
    }

    #[test]
    fn a_single_file_hashes_by_content_wherever_it_lives() {
        let a = tree(&[("f", "hello"), ("g", "hello"), ("h", "other")]);
        let b = tree(&[("f", "hello")]);
        let f = hash_path(&a.path().join("f")).unwrap();
        assert_eq!(
            f,
            hash_path(&b.path().join("f")).unwrap(),
            "same file elsewhere"
        );
        assert_eq!(
            f,
            hash_path(&a.path().join("g")).unwrap(),
            "the name is not hashed"
        );
        assert_ne!(f, hash_path(&a.path().join("h")).unwrap(), "content is");
        assert_ne!(f, hash_path(b.path()).unwrap(), "a file is not its folder");
        assert!(hash_path(&a.path().join("missing")).is_err());
    }
}
