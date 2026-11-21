//! Drift detection: have the devcontainer's inputs (its config, Dockerfile, compose
//! files and local features) changed since this container was created?
//!
//! A fingerprint of the config inputs is stored *inside* the container, so it
//! survives reboots and is discarded automatically when the container is
//! recreated.

use anyhow::{Context, Result};
use json_comments::StripComments;
use std::path::{Path, PathBuf};

use crate::fingerprint::Fingerprint;
use crate::host::Host;
use crate::output::warning;
use crate::runtime::{Container, Env};
use crate::tree::{self, Denied, Links};

use crate::layout::DEVCONTAINER_HASH as HASH_PATH;

/// The hash recorded in the container, running or stopped: `None` if it has
/// none, an error if it could not be read (which must not be mistaken for
/// drift).
pub(super) fn read_hash(container: &dyn Container) -> Result<Option<String>> {
    Ok(container
        .read_stored_file(HASH_PATH)?
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
pub(super) fn config_state(container: &dyn Container, current: &str) -> ConfigState {
    match read_hash(container) {
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

// Hashes what the container is built from, and nothing else: the config file,
// the `extra` files the backend declares (the Dockerfile, the compose files), and
// each local feature folder (`features`: its reference as written, and the folder).
// Not the rest of `.devcontainer/`: a hook or a tool may write there, and neither
// changes the container. (The Dockerfile's build context is not hashed either; a
// rebuild picks up what it copies in.) The files are read from the project's host
// (copied over ssh with `--remote`) and hashed here, so local and remote projects
// get the same answer for the same files.
pub(super) fn hash_inputs(
    config_path: &Path,
    extra: &[PathBuf],
    features: &[(String, PathBuf)],
    host: &dyn Host,
) -> Result<String> {
    let mut entries: Vec<(String, Vec<u8>)> = vec![];

    // Kept alive until hashing is done: a remote directory is copied into it.
    let scratch = crate::tmpfile::TempDir::new("dchash").context("creating a scratch folder")?;
    for (i, (reference, dir)) in features.iter().enumerate() {
        let local = host.fetch_dir(dir, &scratch.path().join(format!("feature{i}")))?;
        let mut files = vec![];
        collect_dir(&local, &mut files)?;
        entries.extend(
            files
                .into_iter()
                .map(|(name, bytes)| (format!("{reference}/{name}"), bytes)),
        );
    }
    for f in std::iter::once(config_path.to_path_buf()).chain(extra.iter().map(|f| lexical(f))) {
        if let Some(bytes) = host.read_file(&f)? {
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
    use crate::host::LocalHost;
    use crate::runtime::Runtime;
    use crate::runtime::fake::{FakeEnv, FakeRuntime};

    fn project(files: &[(&str, &str)]) -> crate::tmpfile::TempDir {
        let tmp = crate::tmpfile::TempDir::new("dc-hash").unwrap();
        for (f, body) in files {
            let path = tmp.path().join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        tmp
    }

    #[test]
    fn files_nobody_declared_do_not_change_the_hash() {
        let tmp = project(&[
            (".devcontainer/devcontainer.json", "{}"),
            (".devcontainer/Dockerfile", "FROM a\n"),
            (".devcontainer/go/devcontainer.json", "{}"),
        ]);
        let dc = tmp.path().join(".devcontainer");
        let config = dc.join("devcontainer.json");
        let docker = [dc.join("Dockerfile")];
        let hash = || hash_inputs(&config, &docker, &[], &LocalHost).unwrap();
        let before = hash();
        // A sibling config, a script, a hook's cache: none of them is an input.
        std::fs::write(dc.join("go/devcontainer.json"), r#"{"a":1}"#).unwrap();
        std::fs::write(dc.join("setup.sh"), "echo\n").unwrap();
        std::fs::create_dir_all(dc.join(".cache")).unwrap();
        std::fs::write(dc.join(".cache/x"), "y").unwrap();
        assert_eq!(hash(), before);
        std::fs::write(dc.join("Dockerfile"), "FROM b\n").unwrap();
        assert_ne!(hash(), before, "the Dockerfile counts");
        let before = hash();
        std::fs::write(&config, r#"{"image":"x"}"#).unwrap();
        assert_ne!(hash(), before, "the config counts");
    }

    #[test]
    fn a_local_feature_counts_whole_under_the_reference_it_was_given() {
        let tmp = project(&[
            (".devcontainer/devcontainer.json", "{}"),
            (".devcontainer/feat/install.sh", "a"),
        ]);
        let dc = tmp.path().join(".devcontainer");
        let config = dc.join("devcontainer.json");
        let feature = [("./feat".to_string(), dc.join("./feat"))];
        let none = hash_inputs(&config, &[], &[], &LocalHost).unwrap();
        let with = hash_inputs(&config, &[], &feature, &LocalHost).unwrap();
        assert_ne!(with, none);
        std::fs::write(dc.join("feat/install.sh"), "b").unwrap();
        assert_ne!(
            hash_inputs(&config, &[], &feature, &LocalHost).unwrap(),
            with
        );
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
    fn a_declared_file_counts_wherever_it_lives() {
        let tmp = project(&[
            (".devcontainer/devcontainer.json", "{}"),
            ("outside.yml", "x"),
        ]);
        let config = tmp.path().join(".devcontainer/devcontainer.json");
        let outside = [tmp.path().join(".devcontainer/../outside.yml")];
        let none = hash_inputs(&config, &[], &[], &LocalHost).unwrap();
        let with = hash_inputs(&config, &outside, &[], &LocalHost).unwrap();
        assert_ne!(with, none);
        std::fs::write(tmp.path().join("outside.yml"), "y").unwrap();
        assert_ne!(
            hash_inputs(&config, &outside, &[], &LocalHost).unwrap(),
            with
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

    /// A container `c` running in `env`, on an engine.
    fn engine(env: FakeEnv) -> FakeRuntime {
        FakeRuntime::new(FakeEnv::new).with_container("c", env)
    }

    #[test]
    fn stored_hash_round_trips_through_the_container() {
        let rt = engine(FakeEnv::new());
        let c = rt.container("c");
        assert_eq!(
            read_hash(c.as_ref()).unwrap(),
            None,
            "no hash before the first create"
        );
        write_hash(c.as_ref(), "00ff11ee22dd33cc").unwrap();
        assert_eq!(
            read_hash(c.as_ref()).unwrap().as_deref(),
            Some("00ff11ee22dd33cc")
        );
    }

    #[test]
    fn an_unreadable_hash_is_an_error_not_a_missing_one() {
        let rt = engine(FakeEnv::new().with_failure(&crate::shell::script_argv(
            crate::runtime::READ_FILE_SCRIPT,
            &[HASH_PATH],
        )));
        let c = rt.container("c");
        assert!(read_hash(c.as_ref()).is_err());
        assert_eq!(config_state(c.as_ref(), "aa"), ConfigState::Unknown);
    }

    #[test]
    fn only_a_different_recorded_hash_is_drift_running_or_stopped() {
        let rt = engine(FakeEnv::new());
        let c = rt.container("c");
        assert_eq!(
            config_state(c.as_ref(), "aa"),
            ConfigState::Unknown,
            "none recorded"
        );
        write_hash(c.as_ref(), "aa").unwrap();
        assert_eq!(config_state(c.as_ref(), "aa"), ConfigState::UpToDate);
        rt.stop("c").unwrap();
        assert_eq!(config_state(c.as_ref(), "bb"), ConfigState::Changed);
    }
}
