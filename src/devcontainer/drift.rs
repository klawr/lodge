//! Drift detection: have the devcontainer's inputs (`.devcontainer/`, and the
//! files it names outside it) changed since this container was created?
//!
//! A fingerprint of the config inputs is stored *inside* the container, so it
//! survives reboots and is discarded automatically when the container is
//! recreated.

use anyhow::{Context, Result};
use json_comments::StripComments;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::fingerprint::Fingerprint;
use crate::output::warning;
use crate::runtime::Env;
use crate::target::Target;
use crate::tree::{self, Denied, Links};

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

/// Whether the config changed since the container was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigState {
    UpToDate,
    Changed,
    /// The container has no recorded config hash, or it could not be read.
    /// Not knowing is no reason to throw a container away, so this is not drift.
    Unknown,
}

/// Compares the hash recorded in the container with `current`, the hash of
/// the config as it is now. The one place drift is decided, for `ldg up` and
/// `ldg status` alike.
pub(super) fn config_state(env: &dyn Env, current: &str) -> ConfigState {
    match read_hash(env) {
        Ok(Some(stored)) if stored == current => ConfigState::UpToDate,
        Ok(Some(_)) => ConfigState::Changed,
        Ok(None) => ConfigState::Unknown,
        Err(e) => {
            warning!("could not read the config recorded in the container: {e:#}");
            ConfigState::Unknown
        }
    }
}

// ── change detection ───────────────────────────────────────────────────────────

// Hashes the inputs that define the container: the folder the config lives in
// when that is `.devcontainer/` or one of its subfolders
// (`.devcontainer/<name>/devcontainer.json`), with everything in it (Dockerfile,
// compose file, scripts, …); otherwise the standalone `.devcontainer.json`. Plus
// any `extra` files the backend declares (a Dockerfile or compose files kept
// elsewhere). The files are read from the project's host (copied over ssh with
// `--remote`) and hashed here, so local and remote projects get the same answer
// for the same files.
pub(super) fn hash_inputs(
    config_path: &Path,
    extra: &[PathBuf],
    target: &Target,
) -> Result<String> {
    let mut entries: Vec<(String, Vec<u8>)> = vec![];
    let devcontainer_dir = config_folder(config_path);

    let mut files: Vec<PathBuf> = Vec::new();
    // Kept alive until hashing is done: a remote directory is copied into it.
    let scratch = crate::tmpfile::TempDir::new("dchash").context("creating a scratch folder")?;
    if let Some(root) = devcontainer_dir {
        let local = target.fetch_dir(root, &scratch.path().join("dir"))?;
        collect_dir(&local, &mut entries)?;
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

    let mut h = Fingerprint::new();
    for (name, bytes) in &entries {
        h.field(name.as_bytes()).put(bytes).put(&[0xff]);
    }
    Ok(h.hex())
}

/// The folder hashed whole for the config at `config_path`: `.devcontainer/`,
/// or the `.devcontainer/<name>/` it is in. `None` for a standalone
/// `.devcontainer.json`.
fn config_folder(config_path: &Path) -> Option<&Path> {
    let dir = config_path.parent()?;
    let is_devcontainer = |p: &Path| p.file_name() == Some(OsStr::new(".devcontainer"));
    (is_devcontainer(dir) || dir.parent().is_some_and(is_devcontainer)).then_some(dir)
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

/// Every file under `root` as (relative path, normalized bytes), as a program
/// reading the folder sees it: symlinks are followed, and a folder or file
/// lodge may not read is left out (it can't be what the container is built from).
fn collect_dir(root: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    tree::walk(root, Links::Follow, Denied::Skip, &mut |e| {
        if !e.meta.is_dir()
            && let Ok(bytes) = std::fs::read(e.path)
        {
            out.push((e.relative.to_string(), normalize_for_hash(e.path, bytes)));
        }
        Ok(())
    })
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
    fn the_hash_is_stable_across_releases() {
        // Recorded in every container: a change here would make every existing
        // container look changed.
        let t = crate::tmpfile::TempDir::new("dc-golden").unwrap();
        let dc = t.path().join("proj/.devcontainer");
        std::fs::create_dir_all(dc.join("sub")).unwrap();
        std::fs::write(dc.join("devcontainer.json"), "{\"image\": \"x\" /* c */}").unwrap();
        std::fs::write(dc.join("Dockerfile"), "FROM x\n").unwrap();
        std::fs::write(dc.join("sub/s.sh"), "echo\n").unwrap();
        std::os::unix::fs::symlink("Dockerfile", dc.join("link")).unwrap();
        assert_eq!(
            hash_inputs(&dc.join("devcontainer.json"), &[], &Target::Local).unwrap(),
            "2b104ffc92d7fc62"
        );
    }

    #[test]
    fn a_subfolder_config_hashes_its_folder_not_its_siblings() {
        let tmp = crate::tmpfile::TempDir::new("dc-sub").unwrap();
        let dc = tmp.path().join(".devcontainer");
        for (f, body) in [
            ("py/devcontainer.json", "{}"),
            ("py/Dockerfile", "FROM a\n"),
            ("go/devcontainer.json", "{}"),
        ] {
            std::fs::create_dir_all(dc.join(f).parent().unwrap()).unwrap();
            std::fs::write(dc.join(f), body).unwrap();
        }
        let config = dc.join("py/devcontainer.json");
        let before = hash_inputs(&config, &[], &Target::Local).unwrap();
        std::fs::write(dc.join("go/devcontainer.json"), r#"{"a":1}"#).unwrap();
        assert_eq!(hash_inputs(&config, &[], &Target::Local).unwrap(), before);
        std::fs::write(dc.join("py/Dockerfile"), "FROM b\n").unwrap();
        assert_ne!(
            hash_inputs(&config, &[], &Target::Local).unwrap(),
            before,
            "the Dockerfile beside the config counts"
        );
    }

    #[test]
    fn only_devcontainer_folders_are_hashed_whole() {
        assert_eq!(
            config_folder(Path::new("/p/.devcontainer/devcontainer.json")),
            Some(Path::new("/p/.devcontainer"))
        );
        assert_eq!(
            config_folder(Path::new("/p/.devcontainer/py/devcontainer.json")),
            Some(Path::new("/p/.devcontainer/py"))
        );
        assert_eq!(config_folder(Path::new("/p/.devcontainer.json")), None);
    }

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
        assert_eq!(config_state(&env, "aa"), ConfigState::Unknown);
    }

    #[test]
    fn only_a_different_recorded_hash_is_drift() {
        let env = FakeEnv::new();
        assert_eq!(
            config_state(&env, "aa"),
            ConfigState::Unknown,
            "none recorded"
        );
        write_hash(&env, "aa").unwrap();
        assert_eq!(config_state(&env, "aa"), ConfigState::UpToDate);
        assert_eq!(config_state(&env, "bb"), ConfigState::Changed);
    }
}
