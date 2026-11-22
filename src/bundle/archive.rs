//! Writing a bundle as a tar file (reading one is [`super::reader`]).
//!
//! The tar is plain (no compression) so anyone can inspect it with ordinary
//! tools. The manifest is written first, so reading it never needs to scan the
//! whole archive.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

use super::manifest::{BIN_DIR, DATA_DIR, LIB_DIR, MANIFEST_FILE};

/// Writes the bundle directory `dir` (a manifest plus `bin/`, `lib/`, `config/`)
/// as a tar.
pub fn write_tar(dir: &Path, out: impl Write) -> Result<()> {
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    builder
        .append_path_with_name(dir.join(MANIFEST_FILE), MANIFEST_FILE)
        .context("adding the manifest")?;
    for sub in [BIN_DIR, LIB_DIR, DATA_DIR] {
        let path = dir.join(sub);
        if path.exists() {
            builder
                .append_dir_all(sub, &path)
                .with_context(|| format!("adding {sub}/"))?;
        }
    }
    builder.finish().context("finishing the archive")
}

#[cfg(test)]
mod tests {
    use super::super::testing::{bundle_dir, tar_of};
    use super::*;

    #[test]
    fn the_manifest_comes_first_so_reading_it_never_scans_the_archive() {
        let (dir, _) = bundle_dir();
        let (_keep, file) = tar_of(dir.path(), "first");
        let mut archive = tar::Archive::new(std::fs::File::open(file).unwrap());
        let first = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(first.path().unwrap().to_string_lossy(), MANIFEST_FILE);
    }
}
