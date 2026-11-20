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

use crate::fingerprint::{FNV_OFFSET, fnv1a};
use crate::runtime::Env;
use crate::target::Target;

use crate::layout::DEVCONTAINER_HASH as HASH_PATH;

/// The hash recorded in the container: `None` if it has none, an error if it
/// could not be read (which must not be mistaken for drift).
pub(super) fn read_hash(env: &dyn Env) -> Result<Option<String>> {
    Ok(env
        .read_file(HASH_PATH)?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

pub(super) fn write_hash(env: &dyn Env, hash: &str) -> Result<()> {
    env.write_file(HASH_PATH, hash.as_bytes())
}

// ── change detection ───────────────────────────────────────────────────────────

// Hashes the inputs that define the container: the whole `.devcontainer/`
// directory when the config lives in one (Dockerfile, compose file, scripts, …),
// otherwise the standalone config, plus any `extra` files the backend declares
// (compose files kept elsewhere). The files are read from the project's host
// (copied over ssh with `--remote`) and hashed here, so local and remote
// projects get the same answer for the same files.
pub(super) fn hash_inputs(
    config_path: &Path,
    extra: &[PathBuf],
    target: &Target,
) -> Result<String> {
    let mut entries: Vec<(String, Vec<u8>)> = vec![];
    let devcontainer_dir = config_path
        .parent()
        .filter(|p| p.file_name() == Some(OsStr::new(".devcontainer")));

    let mut files: Vec<PathBuf> = Vec::new();
    // Kept alive until hashing is done: a remote directory is copied into it.
    let scratch = crate::tmpfile::TempDir::new("dchash").context("creating a scratch folder")?;
    if let Some(root) = devcontainer_dir {
        let local = target.fetch_dir(root, &scratch.path().join("dir"))?;
        collect_dir(&local, &local, &mut entries)?;
    } else {
        files.push(config_path.to_path_buf());
    }
    // Extra files outside the hashed directory (inside it, they already count).
    files.extend(
        extra
            .iter()
            .map(|f| lexical(f))
            .filter(|f| devcontainer_dir.is_none_or(|root| !f.starts_with(lexical(root)))),
    );
    for f in files {
        if let Some(bytes) = target.read_file(&f)? {
            let name = f
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            entries.push((name, normalize_for_hash(&f, bytes)));
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

/// `path` with `.` and `dir/..` folded away, without touching the filesystem
/// (it may be on another host): `/p/.devcontainer/../c.yml` is `/p/c.yml`.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if out.file_name().is_some() => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn paths_fold_lexically() {
        assert_eq!(
            lexical(Path::new("/p/.devcontainer/../c.yml")),
            Path::new("/p/c.yml")
        );
        assert_eq!(lexical(Path::new("/p/./a/b")), Path::new("/p/a/b"));
    }

    #[test]
    fn a_compose_file_outside_the_folder_counts_and_one_inside_counts_once() {
        let tmp = crate::tmpfile::TempDir::new("hash-extra").unwrap();
        let dc = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();
        std::fs::write(dc.join("devcontainer.json"), "{}").unwrap();
        std::fs::write(dc.join("compose.yml"), "a").unwrap();
        std::fs::write(tmp.path().join("outside.yml"), "x").unwrap();
        let config = dc.join("devcontainer.json");
        let inside = [dc.join("compose.yml")];
        let outside = [dc.join("../outside.yml")];
        let none = hash_inputs(&config, &[], &Target::Local).unwrap();
        assert_eq!(hash_inputs(&config, &inside, &Target::Local).unwrap(), none);
        let with_outside = hash_inputs(&config, &outside, &Target::Local).unwrap();
        assert_ne!(with_outside, none);
        std::fs::write(tmp.path().join("outside.yml"), "y").unwrap();
        assert_ne!(
            hash_inputs(&config, &outside, &Target::Local).unwrap(),
            with_outside
        );
    }

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
        assert_eq!(
            read_hash(&env).unwrap(),
            None,
            "no hash before the first create"
        );
        write_hash(&env, "00ff11ee22dd33cc").unwrap();
        assert_eq!(
            read_hash(&env).unwrap().as_deref(),
            Some("00ff11ee22dd33cc")
        );
    }

    #[test]
    fn an_unreadable_hash_is_an_error_not_a_missing_one() {
        let env =
            FakeEnv::new().with_failure(&["sh", "-c", crate::runtime::READ_FILE_SCRIPT, HASH_PATH]);
        assert!(read_hash(&env).is_err());
    }
}
