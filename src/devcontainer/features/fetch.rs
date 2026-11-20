//! Getting a feature's files onto this machine, so they can be copied into the
//! container: OCI references are pulled with `oras`, local paths are read from
//! the project's host (over SSH with `--remote`).

use super::{Context, FeatureMeta};
use crate::host_tools;
use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};

/// A local feature is a relative or absolute path rather than an OCI reference.
pub fn is_local_ref(reference: &str) -> bool {
    reference.starts_with("./") || reference.starts_with("../") || reference.starts_with('/')
}

/// Fetches `reference` into `work` (if it has to be copied) and returns its
/// metadata and the local directory holding `install.sh`.
pub(super) fn fetch(ctx: &Context, reference: &str, work: &Path) -> Result<(FeatureMeta, PathBuf)> {
    let dir = if is_local_ref(reference) {
        // Relative to the devcontainer folder, on the project's host: with
        // `--remote` that is the remote machine, so the folder is copied over.
        ctx.target
            .fetch_dir(&ctx.base_dir.join(reference), work)
            .with_context(|| format!("reading local feature {reference}"))?
    } else {
        pull(reference, work)?
    };
    read_meta(&dir).map(|meta| (meta, dir))
}

/// Reads and checks the feature's metadata. It comes from a registry or
/// someone else's repository, and its `id` names a folder lodge replaces as
/// root, so the id must be a plain name.
fn read_meta(dir: &Path) -> Result<FeatureMeta> {
    let path = dir.join("devcontainer-feature.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let meta: FeatureMeta =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if !crate::shell::is_plain_name(&meta.id) {
        bail!(
            "{} has an unusable id {:?} (letters, digits and -_.+ only)",
            path.display(),
            meta.id
        );
    }
    Ok(meta)
}

/// `oras pull` drops the feature's layer (a plain tar, per the features spec)
/// into `work`; unpack it and return the directory holding `install.sh`.
fn pull(reference: &str, work: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(work)?;
    let mut cmd = host_tools::ORAS.command();
    cmd.args(["pull", reference]).current_dir(work);
    crate::output::trace(&cmd);
    let status = cmd.status().context("running oras pull")?;
    if !status.success() {
        bail!("oras pull failed for feature {reference}");
    }

    let layer = std::fs::read_dir(work)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_file())
        .with_context(|| format!("oras pull produced no artifact for {reference}"))?;
    let content = work.join("content");
    unpack(&layer, &content).with_context(|| format!("unpacking feature {reference}"))?;
    Ok(content)
}

fn unpack(layer: &Path, dest: &Path) -> Result<()> {
    let bytes = std::fs::read(layer).with_context(|| format!("reading {}", layer.display()))?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        bail!("the layer is gzip-compressed; devcontainer features are published as plain tar");
    }
    std::fs::create_dir_all(dest)?;
    tar::Archive::new(bytes.as_slice())
        .unpack(dest)
        .context("extracting the tar layer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_local_refs() {
        assert!(is_local_ref("./my-feature"));
        assert!(is_local_ref("../shared/feat"));
        assert!(is_local_ref("/abs/feat"));
        assert!(!is_local_ref("ghcr.io/devcontainers/features/node:1"));
    }

    #[test]
    fn a_feature_id_that_could_leave_its_folder_is_refused() {
        let dir = crate::tmpfile::TempDir::new("meta-test").unwrap();
        let write = |id: &str| {
            std::fs::write(
                dir.path().join("devcontainer-feature.json"),
                format!(r#"{{"id": {id:?}}}"#),
            )
            .unwrap()
        };
        write("node");
        assert_eq!(read_meta(dir.path()).unwrap().id, "node");
        for bad in ["../../usr", "a/b", "..", ""] {
            write(bad);
            assert!(read_meta(dir.path()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unpacks_a_plain_tar_and_refuses_gzip() {
        let dir = crate::tmpfile::TempDir::new("fetch-test").unwrap();
        let layer = dir.path().join("layer.tar");
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "install.sh", &b"hi"[..])
            .unwrap();
        std::fs::write(&layer, builder.into_inner().unwrap()).unwrap();
        unpack(&layer, &dir.path().join("out")).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("out/install.sh")).unwrap(),
            b"hi"
        );

        let gz = dir.path().join("layer.tgz");
        std::fs::write(&gz, [0x1f, 0x8b, 0, 0]).unwrap();
        assert!(unpack(&gz, &dir.path().join("out2")).is_err());
    }
}
