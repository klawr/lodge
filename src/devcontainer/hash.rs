//! Change detection: has `.devcontainer/` drifted since this container was
//! created?
//!
//! A fingerprint of the config inputs is stored *inside* the container, so it
//! survives reboots and is discarded automatically when the container is
//! recreated.

use anyhow::{Context, Result};
use json_comments::StripComments;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::remote;
use crate::fingerprint::{FNV_OFFSET, fnv1a};
use crate::runtime::Env;
use crate::target::Target;

// Where the hash of the .devcontainer inputs is stored *inside* the container,
// so we can tell on the next `ldg up` whether the config drifted since this
// container instance was created. Living in the container means it survives
// reboots and is discarded automatically when the container is recreated.
const HASH_PATH: &str = "/opt/lodge/devcontainer.hash";

pub(super) fn read_hash(env: &dyn Env) -> Option<String> {
    let tmp = std::env::temp_dir().join(format!("lodge-{}-dchash", std::process::id()));
    let hash = env
        .cp_out(HASH_PATH, &tmp.to_string_lossy())
        .ok()
        .and_then(|_| std::fs::read_to_string(&tmp).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    crate::tmpfile::discard_file(&tmp);
    hash
}

pub(super) fn write_hash(env: &dyn Env, hash: &str) -> Result<()> {
    env.write_file(HASH_PATH, hash.as_bytes())
}

// ── change detection ───────────────────────────────────────────────────────────

// Hashes the inputs that define the container. When the config lives in a
// `.devcontainer/` directory we hash the whole directory (Dockerfile, compose
// file, scripts, …); otherwise we hash the standalone config plus any `extra`
// files the backend declares (e.g. compose files referenced from elsewhere).
pub(super) fn hash_inputs(
    config_path: &Path,
    extra: &[PathBuf],
    target: &Target,
) -> Result<String> {
    if let Target::Ssh(host) = target {
        return remote::hash_inputs(host, config_path, extra);
    }

    let mut entries: Vec<(String, Vec<u8>)> = vec![];
    let devcontainer_dir = config_path
        .parent()
        .filter(|p| p.file_name() == Some(OsStr::new(".devcontainer")));

    if let Some(root) = devcontainer_dir {
        collect_dir(root, root, &mut entries)?;
    } else {
        let mut files = vec![config_path.to_path_buf()];
        files.extend_from_slice(extra);
        for f in files {
            if let Ok(bytes) = std::fs::read(&f) {
                let name = f
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                entries.push((name, normalize_for_hash(&f, bytes)));
            }
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut h = FNV_OFFSET;
    for (name, bytes) in &entries {
        h = fnv1a(name.as_bytes(), h);
        h = fnv1a(&[0x00], h);
        h = fnv1a(bytes, h);
        h = fnv1a(&[0xff], h);
    }
    Ok(format!("{h:016x}"))
}

// devcontainer.json is JSONC; canonicalize it before hashing so comment-only
// (and whitespace/key-order) edits don't trigger a spurious recreate prompt.
// Parsing to a serde_json::Value and re-serializing drops comments and
// formatting (serde_json's map is sorted, so key order is normalized too).
// Comment-stripping alone is insufficient: json_comments replaces comments with
// spaces, so a longer comment still changes the bytes. Other files hash as-is.
fn normalize_for_hash(path: &Path, bytes: Vec<u8>) -> Vec<u8> {
    let is_jsonc = matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("devcontainer.json") | Some(".devcontainer.json")
    );
    if is_jsonc
        && let Ok(value) =
            serde_json::from_reader::<_, serde_json::Value>(StripComments::new(bytes.as_slice()))
        && let Ok(canonical) = serde_json::to_vec(&value)
    {
        return canonical;
    }
    bytes
}

fn collect_dir(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut items: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    items.sort();
    for path in items {
        if path.is_dir() {
            collect_dir(root, &path, out)?;
        } else if let Ok(bytes) = std::fs::read(&path) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((rel, normalize_for_hash(&path, bytes)));
        }
    }
    Ok(())
}

/// Short, stable tag derived from a project's path. Appended to container and
/// session names so two projects that share a basename never collide.
pub(super) fn path_tag(path: &Path) -> String {
    let h = fnv1a(path.to_string_lossy().as_bytes(), FNV_OFFSET);
    format!("{:06x}", h & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn hash_normalization_ignores_comments_and_formatting() {
        let dc = Path::new("devcontainer.json");
        let pretty = normalize_for_hash(dc, br#"{ /* hi */ "a": 1, "b": 2 } // x"#.to_vec());
        let compact = normalize_for_hash(dc, br#"{"b":2,"a":1}"#.to_vec());
        assert_eq!(
            pretty, compact,
            "comments/whitespace/key-order must not affect the hash"
        );
    }

    #[test]
    fn hash_normalization_keeps_value_changes() {
        let dc = Path::new("devcontainer.json");
        let a = normalize_for_hash(dc, br#"{"a":1}"#.to_vec());
        let b = normalize_for_hash(dc, br#"{"a":2}"#.to_vec());
        assert_ne!(a, b);
    }

    #[test]
    fn hash_leaves_non_json_bytes_untouched() {
        let bytes = b"FROM debian\n// not a comment in a Dockerfile\n".to_vec();
        assert_eq!(
            normalize_for_hash(Path::new("Dockerfile"), bytes.clone()),
            bytes
        );
    }

    #[test]
    fn stored_hash_round_trips_through_the_environment() {
        let env = FakeEnv::new();
        assert_eq!(read_hash(&env), None, "no hash before the first create");
        write_hash(&env, "00ff11ee22dd33cc").unwrap();
        assert_eq!(read_hash(&env).as_deref(), Some("00ff11ee22dd33cc"));
    }
}
