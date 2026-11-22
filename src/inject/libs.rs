use anyhow::{Context, Result};
use std::path::Path;

use crate::bundle::manifest::LIB_DIR;
use crate::bundle::source::Prepared;

/// Puts the bundle's libraries (and the dynamic linker) into `stage/lib/`, which
/// is sent as a whole to the prefix.
///
/// The linker lives there too, as one copy: binaries name it as their interpreter
/// and find their libraries next to it, so libc and the linker always move
/// together.
pub(super) fn stage_libs(bundle: &Prepared, stage: &Path) -> Result<()> {
    let libs = &bundle.manifest.libs;
    if libs.is_empty() {
        return Ok(());
    }
    let dir = stage.join(LIB_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    for lib in libs {
        let from = bundle.lib(&lib.name);
        let to = dir.join(&lib.name);
        // A link costs nothing; a copy is for a bundle cache on another filesystem.
        if std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to).with_context(|| format!("staging {}", from.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{Lib, Manifest};
    use crate::bundle::source::Origin;
    use crate::tmpfile::TempDir;

    /// A bundle directory holding the given libraries, the first being the linker.
    fn bundle_with(libs: &[(&str, &str)]) -> (TempDir, Prepared) {
        let dir = TempDir::new("libs-bundle").unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        for (name, contents) in libs {
            std::fs::write(dir.path().join("lib").join(name), contents).unwrap();
        }
        let manifest = Manifest {
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: libs.first().map(|(n, _)| n.to_string()),
            libs: libs
                .iter()
                .map(|(n, c)| Lib {
                    name: n.to_string(),
                    hash: format!("{:016x}", c.len()),
                })
                .collect(),
            binaries: vec![],
            data: vec![],
            git_safe_directories: vec![],
        };
        let prepared = Prepared {
            dir: dir.path().to_path_buf(),
            manifest,
            origin: Origin::Packed,
            source: None,
        };
        (dir, prepared)
    }

    #[test]
    fn libraries_and_the_linker_are_staged_together_in_lib() {
        let (_dir, bundle) = bundle_with(&[("ld-linux.so.2", "linker"), ("libc.so.6", "libc")]);
        let stage = TempDir::new("libs-stage").unwrap();
        stage_libs(&bundle, stage.path()).unwrap();
        let read = |n: &str| std::fs::read_to_string(stage.path().join("lib").join(n)).unwrap();
        assert_eq!(read("libc.so.6"), "libc");
        assert_eq!(read("ld-linux.so.2"), "linker");
    }

    #[test]
    fn a_bundle_without_libraries_stages_nothing() {
        let (_dir, bundle) = bundle_with(&[]);
        let stage = TempDir::new("libs-none").unwrap();
        stage_libs(&bundle, stage.path()).unwrap();
        assert!(!stage.path().join("lib").exists());
    }
}
