//! Shared libraries and the dynamic linker.
//!
//! Tracks what was copied by content hash, so a library that changed on the host
//! (same name, new content) is refreshed rather than skipped.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{LODGE_DIR, LODGE_LIB};
use crate::deps;
use crate::output::info;
use crate::runtime::Env;
// Copies all shared library deps (including the dynamic linker) into
// /opt/lodge/lib/, skipping ones already there. Returns the linker filename
// (e.g. "ld-linux-x86-64.so.2") so the wrapper can launch the binary through it.
// Copies the binary's shared libraries (and the dynamic linker) into LODGE_LIB,
// and returns the linker's file name.
//
// A library is copied when its *content* differs from what a previous injection
// recorded in LIB_MANIFEST, not merely when its name is absent. Names don't
// change across versions (`libc.so.6` is `libc.so.6` in every glibc), and a
// container outlives host upgrades, so skipping by name would leave an old libc
// beside a newer linker or binary, which crashes at startup.
//
// The linker is also kept at LODGE_DIR for the patched binaries' PT_INTERP. When
// it is refreshed there it must move together with libc, or binaries injected
// earlier would end up with a linker and a libc from different glibc versions.
pub(super) fn inject_deps(env: &dyn Env, binary: &str, name: &str) -> Result<Option<String>> {
    let deps = deps::resolve(binary)?;
    if deps.is_empty() {
        return Ok(None);
    }

    let linker_name = deps
        .linker
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());

    let mut libs = Vec::with_capacity(deps.libs.len());
    for dep in &deps.libs {
        let Some(file_name) = dep.file_name() else {
            continue;
        };
        // Resolve symlinks so we copy the real .so, then place it under its
        // requested name so the linker's library-path search finds it.
        let real = std::fs::canonicalize(dep).unwrap_or_else(|_| dep.to_path_buf());
        let bytes = std::fs::read(&real).with_context(|| format!("reading {}", real.display()))?;
        libs.push(LibFile {
            name: file_name.to_string_lossy().into_owned(),
            hash: crate::fingerprint::content_hash(&bytes),
            real,
        });
    }

    let mut manifest =
        parse_manifest(&env.exec_capture(&["cat", LIB_MANIFEST]).unwrap_or_default());
    let changed = changed_libs(&libs, &manifest);
    if changed.is_empty() {
        return Ok(linker_name);
    }

    info!("copying {} changed deps for {}", changed.len(), name);
    for lib in &changed {
        env.cp_in(
            &lib.real.to_string_lossy(),
            &format!("{LODGE_LIB}/{}", lib.name),
        )?;
        manifest.insert(lib.name.clone(), lib.hash.clone());
    }
    env.write_file(LIB_MANIFEST, render_manifest(&manifest).as_bytes())?;

    if let Some(ld) = &linker_name
        && changed.iter().any(|l| &l.name == ld)
    {
        env.exec_root(&[
            "cp",
            &format!("{LODGE_LIB}/{ld}"),
            &format!("{LODGE_DIR}/{ld}"),
        ])?;
    }

    Ok(linker_name)
}

/// A host library about to be copied: the name it is requested by, the file
/// that backs it, and a fingerprint of its content.
struct LibFile {
    name: String,
    real: PathBuf,
    hash: String,
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
fn changed_libs<'a>(libs: &'a [LibFile], manifest: &BTreeMap<String, String>) -> Vec<&'a LibFile> {
    libs.iter()
        .filter(|l| manifest.get(&l.name) != Some(&l.hash))
        .collect()
}

// What we copied into LODGE_LIB and what it looked like. See `inject_deps`.
const LIB_MANIFEST: &str = "/opt/lodge/lib/.manifest";

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(name: &str, hash: &str) -> LibFile {
        LibFile {
            name: name.into(),
            real: format!("/host/{name}").into(),
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
}
