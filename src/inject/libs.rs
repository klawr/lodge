//! Shared libraries and the dynamic linker.
//!
//! Copied from the bundle, tracked by content hash: a library that changed (same
//! name, new content) is refreshed rather than skipped. Names don't change across
//! versions (`libc.so.6` is `libc.so.6` in every glibc), and a container outlives
//! upgrades, so skipping by name would leave an old libc beside a newer linker or
//! binary, which crashes at startup.

use anyhow::Result;
use std::collections::BTreeMap;

use super::{LODGE_DIR, LODGE_LIB};
use crate::bundle::manifest::Lib;
use crate::bundle::source::Prepared;
use crate::output::info;
use crate::runtime::Env;

// What we copied into LODGE_LIB and what it looked like.
const LIB_MANIFEST: &str = "/opt/lodge/lib/.manifest";

/// Copies the bundle's libraries (and the dynamic linker) into `/opt/lodge/lib`
/// when their content differs from what an earlier injection recorded.
///
/// The linker is also kept at `/lodge` for the relinked binaries' interpreter.
/// When it is refreshed there it must move together with libc, or binaries
/// injected earlier would end up with a linker and a libc from different glibc
/// versions.
pub(super) fn inject_libs(env: &dyn Env, bundle: &Prepared) -> Result<()> {
    let libs = &bundle.manifest.libs;
    if libs.is_empty() {
        return Ok(());
    }

    let mut installed =
        parse_manifest(&env.exec_capture(&["cat", LIB_MANIFEST]).unwrap_or_default());
    let changed = changed_libs(libs, &installed);
    if !changed.is_empty() {
        info!(
            "copying {} changed librar{}",
            changed.len(),
            if changed.len() == 1 { "y" } else { "ies" }
        );
        for lib in &changed {
            env.cp_in(
                &bundle.lib(&lib.name).to_string_lossy(),
                &format!("{LODGE_LIB}/{}", lib.name),
            )?;
            installed.insert(lib.name.clone(), lib.hash.clone());
        }
        env.write_file(LIB_MANIFEST, render_manifest(&installed).as_bytes())?;
    }

    if let Some(ld) = &bundle.manifest.linker {
        let placed = format!("{LODGE_DIR}/{ld}");
        if changed.iter().any(|l| &l.name == ld) || !env.file_exists(&placed) {
            env.exec_root(&["cp", &format!("{LODGE_LIB}/{ld}"), &placed])?;
        }
    }
    Ok(())
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
    use crate::bundle::manifest::{FORMAT, Manifest};
    use crate::bundle::source::Origin;
    use crate::runtime::fake::FakeEnv;
    use crate::tmpfile::TempDir;

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
            format: FORMAT,
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: libs.first().map(|(n, _)| n.to_string()),
            libs: libs
                .iter()
                .map(|(n, c)| lib(n, &format!("{:016x}", c.len())))
                .collect(),
            tools: vec![],
            aliases: BTreeMap::new(),
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
    fn libraries_are_copied_once_and_the_linker_is_placed() {
        let (_dir, bundle) = bundle_with(&[("ld-linux.so.2", "linker"), ("libc.so.6", "libc")]);
        let env = FakeEnv::new();
        inject_libs(&env, &bundle).unwrap();

        assert_eq!(env.file("/opt/lodge/lib/libc.so.6").unwrap(), "libc");
        assert!(
            env.file("/opt/lodge/lib/.manifest")
                .unwrap()
                .contains("libc.so.6")
        );
        assert!(env.ran(&["cp", "/opt/lodge/lib/ld-linux.so.2", "/lodge/ld-linux.so.2"]));
    }

    #[test]
    fn an_unchanged_bundle_copies_nothing_the_second_time() {
        let (_dir, bundle) = bundle_with(&[("ld-linux.so.2", "linker"), ("libc.so.6", "libc")]);
        let env = FakeEnv::new();
        inject_libs(&env, &bundle).unwrap();

        // A second run against the container as the first left it.
        let again = FakeEnv::new()
            .with_file(
                "/opt/lodge/lib/.manifest",
                &env.file("/opt/lodge/lib/.manifest").unwrap(),
            )
            .with_file("/lodge/ld-linux.so.2", "linker");
        inject_libs(&again, &bundle).unwrap();
        assert!(
            again.file("/opt/lodge/lib/libc.so.6").is_none(),
            "libc was copied again"
        );
        assert!(!again.ran(&["cp", "/opt/lodge/lib/ld-linux.so.2", "/lodge/ld-linux.so.2"]));
    }

    #[test]
    fn a_missing_linker_copy_is_restored_even_when_nothing_changed() {
        let (_dir, bundle) = bundle_with(&[("ld-linux.so.2", "linker")]);
        let first = FakeEnv::new();
        inject_libs(&first, &bundle).unwrap();
        let manifest = first.file("/opt/lodge/lib/.manifest").unwrap();

        // Same manifest and library, but /lodge/<ld> is gone.
        let env = FakeEnv::new()
            .with_file("/opt/lodge/lib/.manifest", &manifest)
            .with_file("/opt/lodge/lib/ld-linux.so.2", "linker");
        inject_libs(&env, &bundle).unwrap();
        assert!(env.ran(&["cp", "/opt/lodge/lib/ld-linux.so.2", "/lodge/ld-linux.so.2"]));
    }

    #[test]
    fn a_bundle_without_libraries_touches_nothing() {
        let (_dir, bundle) = bundle_with(&[]);
        let env = FakeEnv::new();
        inject_libs(&env, &bundle).unwrap();
        assert!(env.file("/opt/lodge/lib/.manifest").is_none());
    }
}
