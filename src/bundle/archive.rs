//! Reading and writing a bundle as a tar file.
//!
//! The tar is plain (no compression) so anyone can inspect it with ordinary
//! tools. The manifest is written first, so reading it never needs to scan the
//! whole archive.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::Path;

use super::hash::hash_path;
use super::manifest::{BIN_DIR, CONFIG_DIR, LIB_DIR, MANIFEST_FILE, Manifest};

/// Writes the bundle directory `dir` (a manifest plus `bin/`, `lib/`, `config/`)
/// as a tar.
pub fn write_tar(dir: &Path, out: impl Write) -> Result<()> {
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    builder
        .append_path_with_name(dir.join(MANIFEST_FILE), MANIFEST_FILE)
        .context("adding the manifest")?;
    for sub in [BIN_DIR, LIB_DIR, CONFIG_DIR] {
        let path = dir.join(sub);
        if path.exists() {
            builder
                .append_dir_all(sub, &path)
                .with_context(|| format!("adding {sub}/"))?;
        }
    }
    builder.finish().context("finishing the archive")
}

/// Reads just the manifest of the bundle at `file`, without extracting it.
pub fn read_manifest(file: &Path) -> Result<Manifest> {
    let handle =
        std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut archive = tar::Archive::new(handle);
    for entry in archive.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        if entry.path()?.as_ref() == Path::new(MANIFEST_FILE) {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Manifest::from_json(&bytes);
        }
    }
    bail!(
        "{} is not a lodge bundle: it has no {MANIFEST_FILE}",
        file.display()
    )
}

/// Extracts the bundle at `file` into `dest` and verifies it against its
/// manifest, which is returned.
///
/// The tar crate refuses entries that would land outside `dest`; the manifest's
/// own paths are derived from names (see [`Manifest`]), so nothing in it can point
/// elsewhere either.
pub fn extract(file: &Path, dest: &Path) -> Result<Manifest> {
    let handle =
        std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let mut archive = tar::Archive::new(handle);
    archive.set_unpack_xattrs(false);
    archive
        .unpack(dest)
        .with_context(|| format!("extracting {}", file.display()))?;
    let manifest = Manifest::from_json(
        &std::fs::read(dest.join(MANIFEST_FILE)).context("the bundle has no manifest")?,
    )?;
    verify(dest, &manifest)?;
    Ok(manifest)
}

/// Checks every file the manifest lists against its recorded hash, so a bundle
/// damaged in transit is caught before anything is installed from it.
pub fn verify(dir: &Path, manifest: &Manifest) -> Result<()> {
    let check = |relative: String, expected: &str| -> Result<()> {
        let actual = hash_path(&dir.join(&relative))
            .with_context(|| format!("the bundle is missing {relative}"))?;
        if actual != expected {
            bail!("the bundle is damaged: {relative} does not match its recorded hash");
        }
        Ok(())
    };
    for lib in &manifest.libs {
        check(format!("{LIB_DIR}/{}", lib.name), &lib.hash)?;
    }
    for tool in &manifest.tools {
        if let Some(b) = &tool.binary {
            check(format!("{BIN_DIR}/{}", tool.name), &b.hash)?;
        }
        if let Some(c) = &tool.config {
            check(format!("{CONFIG_DIR}/{}", tool.name), &c.hash)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::manifest::{Binary, ConfigTree, FORMAT, Lib, Tool};
    use super::*;
    use crate::tmpfile::TempDir;

    /// A small but complete bundle directory, with its manifest.
    fn bundle_dir() -> (TempDir, Manifest) {
        let dir = TempDir::new("bundle").unwrap();
        let p = dir.path();
        for sub in [BIN_DIR, LIB_DIR, CONFIG_DIR] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        std::fs::write(p.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(p.join("lib/libx.so.1"), "library").unwrap();
        std::fs::create_dir_all(p.join("config/hello/sub")).unwrap();
        std::fs::write(p.join("config/hello/sub/rc"), "set x").unwrap();

        let manifest = Manifest {
            format: FORMAT,
            lodge_version: "0".into(),
            created: 1,
            arch: "x86_64".into(),
            linker: Some("libx.so.1".into()),
            libs: vec![Lib {
                name: "libx.so.1".into(),
                hash: hash_path(&p.join("lib/libx.so.1")).unwrap(),
            }],
            tools: vec![Tool {
                name: "hello".into(),
                binary: Some(Binary {
                    dest: None,
                    dynamic: true,
                    hash: hash_path(&p.join("bin/hello")).unwrap(),
                }),
                config: Some(ConfigTree {
                    dest: "~/.config/hello".into(),
                    is_dir: true,
                    hash: hash_path(&p.join("config/hello")).unwrap(),
                }),
                skip_if_exists: true,
            }],
            git_safe_directories: vec![],
        };
        std::fs::write(p.join(MANIFEST_FILE), manifest.to_json().unwrap()).unwrap();
        (dir, manifest)
    }

    fn tar_of(dir: &Path, name: &str) -> (TempDir, std::path::PathBuf) {
        let out = TempDir::new(name).unwrap();
        let file = out.path().join("b.tar");
        write_tar(dir, std::fs::File::create(&file).unwrap()).unwrap();
        (out, file)
    }

    #[test]
    fn a_bundle_survives_a_round_trip_through_tar() {
        let (dir, manifest) = bundle_dir();
        let (_keep, file) = tar_of(dir.path(), "rt");

        assert_eq!(read_manifest(&file).unwrap(), manifest);

        let dest = TempDir::new("extracted").unwrap();
        assert_eq!(extract(&file, dest.path()).unwrap(), manifest);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("config/hello/sub/rc")).unwrap(),
            "set x"
        );
    }

    #[test]
    fn a_damaged_file_is_caught_on_extraction() {
        let (dir, _) = bundle_dir();
        std::fs::write(dir.path().join("lib/libx.so.1"), "tampered").unwrap();
        let (_keep, file) = tar_of(dir.path(), "damaged");
        let dest = TempDir::new("dest").unwrap();
        let err = extract(&file, dest.path()).unwrap_err().to_string();
        assert!(
            err.contains("damaged") && err.contains("lib/libx.so.1"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_file_is_caught_too() {
        let (dir, _) = bundle_dir();
        std::fs::remove_file(dir.path().join("bin/hello")).unwrap();
        let (_keep, file) = tar_of(dir.path(), "missing");
        let dest = TempDir::new("dest2").unwrap();
        let err = format!("{:#}", extract(&file, dest.path()).unwrap_err());
        assert!(err.contains("bin/hello"), "{err}");
    }

    #[test]
    fn something_that_is_not_a_bundle_says_so() {
        let dir = TempDir::new("notabundle").unwrap();
        let file = dir.path().join("x.tar");
        let mut b = tar::Builder::new(std::fs::File::create(&file).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_cksum();
        b.append_data(&mut header, "readme", &b"hi"[..]).unwrap();
        b.finish().unwrap();
        drop(b);
        let err = read_manifest(&file).unwrap_err().to_string();
        assert!(err.contains("not a lodge bundle"), "{err}");
    }
}
