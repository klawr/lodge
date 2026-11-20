//! A content hash for a file or a whole directory tree.
//!
//! Deterministic: entries are visited in name order, and the hash covers each
//! entry's relative path, kind, the executable bit and the content (a symlink
//! contributes its target). Two trees hash the same exactly when they would
//! install the same.

use anyhow::{Context, Result};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::fingerprint::{FNV_OFFSET, fnv1a};

pub fn hash_path(path: &Path) -> Result<String> {
    let mut h = FNV_OFFSET;
    feed_entry(path, "", &mut h)?;
    Ok(format!("{h:016x}"))
}

fn feed_entry(path: &Path, label: &str, h: &mut u64) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.is_symlink() {
        let target = std::fs::read_link(path)?;
        feed(h, b"L\0");
        feed(h, label.as_bytes());
        feed(h, b"\0");
        feed(h, target.to_string_lossy().as_bytes());
        feed(h, b"\0");
    } else if meta.is_dir() {
        feed(h, b"D\0");
        feed(h, label.as_bytes());
        feed(h, b"\0");
        let mut names: Vec<_> = std::fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<_>>()?;
        names.sort();
        for name in names {
            let child = format!("{label}/{}", name.to_string_lossy());
            feed_entry(&path.join(&name), &child, h)?;
        }
    } else {
        feed(h, b"F\0");
        feed(h, label.as_bytes());
        feed(h, b"\0");
        feed(h, &[u8::from(meta.permissions().mode() & 0o111 != 0)]);
        feed_file(path, h)?;
    }
    Ok(())
}

fn feed(h: &mut u64, data: &[u8]) {
    *h = fnv1a(data, *h);
}

/// Streams a file through the hash, so a large binary is never held in memory.
fn feed_file(path: &Path, h: &mut u64) -> Result<()> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        feed(h, &buf[..n]);
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

    #[test]
    fn any_change_to_content_name_or_mode_changes_the_hash() {
        let base = tree(&[("a", "1")]);
        let h = hash_path(base.path()).unwrap();
        assert_ne!(h, hash_path(tree(&[("a", "2")]).path()).unwrap(), "content");
        assert_ne!(h, hash_path(tree(&[("b", "1")]).path()).unwrap(), "name");
        std::fs::set_permissions(
            base.path().join("a"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_ne!(h, hash_path(base.path()).unwrap(), "executable bit");
    }

    #[test]
    fn a_symlink_counts_by_its_target_not_what_it_points_at() {
        let a = TempDir::new("link-a").unwrap();
        std::os::unix::fs::symlink("one", a.path().join("l")).unwrap();
        let b = TempDir::new("link-b").unwrap();
        std::os::unix::fs::symlink("two", b.path().join("l")).unwrap();
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
