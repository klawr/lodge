//! Bundles for tests: a directory laid out as `ldg pack` leaves it, the tar of
//! one, and a [`Prepared`] built from a few files.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::archive::write_tar;
use super::hash::hash_path;
use super::manifest::{BIN_DIR, Binary, DATA_DIR, Data, LIB_DIR, Lib, MANIFEST_FILE, Manifest};
use super::reader::Bundle;
use super::source::{Origin, Prepared};
use crate::tmpfile::TempDir;

/// A small but complete bundle directory, with its manifest.
pub(crate) fn bundle_dir() -> (TempDir, Manifest) {
    let dir = TempDir::new("bundle").unwrap();
    let p = dir.path();
    for sub in [BIN_DIR, LIB_DIR, DATA_DIR] {
        std::fs::create_dir_all(p.join(sub)).unwrap();
    }
    std::fs::write(p.join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(p.join("bin/hello"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(p.join("lib/libx.so.1"), "library").unwrap();
    std::fs::create_dir_all(p.join("data/0/sub")).unwrap();
    std::fs::write(p.join("data/0/sub/rc"), "set x").unwrap();
    std::fs::write(p.join("data/0/a-b"), "dash").unwrap();
    std::os::unix::fs::symlink("sub/rc", p.join("data/0/link")).unwrap();

    let manifest = Manifest {
        lodge_version: "0".into(),
        created: 1,
        arch: "x86_64".into(),
        linker: Some("libx.so.1".into()),
        libs: vec![Lib {
            name: "libx.so.1".into(),
            hash: hash_path(&p.join("lib/libx.so.1")).unwrap(),
        }],
        binaries: vec![Binary {
            name: "hello".into(),
            dest: None,
            dynamic: true,
            hash: hash_path(&p.join("bin/hello")).unwrap(),
        }],
        data: vec![Data {
            dest: "~/.config/hello".into(),
            is_dir: true,
            hash: hash_path(&p.join("data/0")).unwrap(),
            binary: Some("hello".into()),
        }],
        git_safe_directories: vec![],
    };
    std::fs::write(p.join(MANIFEST_FILE), manifest.to_json().unwrap()).unwrap();
    (dir, manifest)
}

pub(crate) fn tar_of(dir: &Path, name: &str) -> (TempDir, PathBuf) {
    let out = TempDir::new(name).unwrap();
    let file = out.path().join("b.tar");
    write_tar(dir, std::fs::File::create(&file).unwrap()).unwrap();
    (out, file)
}

/// A manifest that lists nothing, for a test to add to.
pub(crate) fn manifest(arch: &str) -> Manifest {
    Manifest {
        lodge_version: "0".into(),
        created: 0,
        arch: arch.into(),
        linker: None,
        libs: Vec::<Lib>::new(),
        binaries: Vec::<Binary>::new(),
        data: Vec::<Data>::new(),
        git_safe_directories: Vec::new(),
    }
}

/// A bundle holding `manifest` and `files` (paths in the bundle, with their
/// contents), as a [`Prepared`] that was just packed. The hashes the manifest
/// carries are not checked: a test that wants them right uses [`bundle_dir`].
pub(crate) fn prepared(manifest: &Manifest, files: &[(&str, &[u8])]) -> (TempDir, Prepared) {
    let dir = TempDir::new("test-bundle").unwrap();
    for (path, contents) in files {
        let at = dir.path().join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, contents).unwrap();
    }
    std::fs::write(dir.path().join(MANIFEST_FILE), manifest.to_json().unwrap()).unwrap();
    // `write_tar` takes only the manifest and the three folders, so the tar can
    // sit in the directory it is of.
    let tar = dir.path().join("bundle.tar");
    write_tar(dir.path(), std::fs::File::create(&tar).unwrap()).unwrap();
    let bundle = Bundle::open(&tar).unwrap();
    (
        dir,
        Prepared {
            bundle,
            origin: Origin::Packed,
            source: None,
        },
    )
}
