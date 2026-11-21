use anyhow::Result;
use std::collections::BTreeMap;

use crate::bundle::manifest::Lib;
use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::output::{info, plural};
use crate::runtime::Env;

/// Copies the bundle's libraries (and the dynamic linker) into the prefix's
/// `lib/` when their content differs from what an earlier injection recorded.
///
/// The linker lives there too, as one copy: binaries name it as their interpreter
/// and find their libraries next to it, so a refresh of the linker and libc
/// always moves together.
pub(super) fn inject_libs(env: &dyn Env, layout: &Layout, bundle: &Prepared) -> Result<()> {
    let libs = &bundle.manifest.libs;
    if libs.is_empty() {
        return Ok(());
    }

    let manifest_path = layout.lib_manifest();
    let mut installed = parse_manifest(&env.read_file(&manifest_path)?.unwrap_or_default());
    let changed = changed_libs(libs, &installed);
    if changed.is_empty() {
        return Ok(());
    }
    info!(
        "copying {}",
        plural(changed.len(), "changed library", "changed libraries")
    );
    for lib in &changed {
        env.cp_in(
            &bundle.lib(&lib.name).to_string_lossy(),
            &layout.lib_file(&lib.name),
        )?;
        installed.insert(lib.name.clone(), lib.hash.clone());
    }
    env.write_file(&manifest_path, render_manifest(&installed).as_bytes())
}

/// Parses the manifest: one `<hash> <file name>` per line.
fn parse_manifest(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(hash, name)| (name.to_string(), hash.to_string()))
        .collect()
}

fn render_manifest(manifest: &BTreeMap<String, String>) -> String {
    manifest
        .iter()
        .map(|(name, hash)| format!("{hash} {name}\n"))
        .collect()
}

/// The libraries whose content differs from what the manifest recorded, or that
/// it never recorded.
fn changed_libs<'a>(libs: &'a [Lib], manifest: &BTreeMap<String, String>) -> Vec<&'a Lib> {
    libs.iter()
        .filter(|l| manifest.get(&l.name) != Some(&l.hash))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::Manifest;
    use crate::bundle::source::Origin;
    use crate::runtime::fake::FakeEnv;
    use crate::tmpfile::TempDir;

    fn layout() -> Layout {
        Layout::new("/p")
    }

    fn lib(name: &str, hash: &str) -> Lib {
        Lib {
            name: name.into(),
            hash: hash.into(),
        }
    }

    #[test]
    fn manifest_round_trips() {
        let text = "aaaa libc.so.6\nbbbb libm.so.6\n";
        let manifest = parse_manifest(text);
        assert_eq!(manifest.get("libc.so.6").map(String::as_str), Some("aaaa"));
        assert_eq!(render_manifest(&manifest), text);
    }

    #[test]
    fn same_name_with_new_content_is_a_changed_library() {
        // The case name-only checks got wrong: a host upgrade keeps the file name.
        let manifest = parse_manifest("aaaa libc.so.6\nbbbb libm.so.6\n");
        let libs = [lib("libc.so.6", "cccc"), lib("libm.so.6", "bbbb")];
        let changed: Vec<&str> = changed_libs(&libs, &manifest)
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(changed, ["libc.so.6"]);
    }

    #[test]
    fn unrecorded_libraries_are_copied_and_unchanged_ones_are_not() {
        let manifest = parse_manifest("aaaa libc.so.6\n");
        let libs = [lib("libc.so.6", "aaaa"), lib("libz.so.1", "dddd")];
        let changed: Vec<&str> = changed_libs(&libs, &manifest)
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(changed, ["libz.so.1"]);
    }

    #[test]
    fn an_empty_manifest_means_everything_is_copied() {
        let libs = [lib("libc.so.6", "aaaa")];
        assert_eq!(changed_libs(&libs, &parse_manifest("")).len(), 1);
    }

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
                .map(|(n, c)| lib(n, &format!("{:016x}", c.len())))
                .collect(),
            tools: vec![],
            git_safe_directories: vec![],
        };
        let prepared = Prepared {
            dir: dir.path().to_path_buf(),
            manifest,
            origin: Origin::Packed,
        };
        (dir, prepared)
    }

    #[test]
    fn libraries_and_the_linker_are_copied_once_into_the_prefix() {
        let (_dir, bundle) = bundle_with(&[("ld-linux.so.2", "linker"), ("libc.so.6", "libc")]);
        let env = FakeEnv::new();
        inject_libs(&env, &layout(), &bundle).unwrap();

        assert_eq!(env.file("/p/lib/libc.so.6").unwrap(), "libc");
        assert_eq!(
            env.file("/p/lib/ld-linux.so.2").unwrap(),
            "linker",
            "the one copy of the linker is next to the libraries"
        );
        assert!(env.file("/p/lib/.manifest").unwrap().contains("libc.so.6"));
    }

    #[test]
    fn an_unchanged_bundle_copies_nothing_the_second_time() {
        let (_dir, bundle) = bundle_with(&[("ld-linux.so.2", "linker"), ("libc.so.6", "libc")]);
        let env = FakeEnv::new();
        inject_libs(&env, &layout(), &bundle).unwrap();

        // A second run against the target as the first left it.
        let again =
            FakeEnv::new().with_file("/p/lib/.manifest", &env.file("/p/lib/.manifest").unwrap());
        inject_libs(&again, &layout(), &bundle).unwrap();
        assert!(
            again.file("/p/lib/libc.so.6").is_none(),
            "libc was copied again"
        );
    }

    #[test]
    fn a_bundle_without_libraries_touches_nothing() {
        let (_dir, bundle) = bundle_with(&[]);
        let env = FakeEnv::new();
        inject_libs(&env, &layout(), &bundle).unwrap();
        assert!(env.file("/p/lib/.manifest").is_none());
    }
}
