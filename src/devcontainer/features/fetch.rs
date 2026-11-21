//! Getting a feature's files onto this machine, so they can be copied into the
//! container: OCI references are pulled through the context's `OciFetcher`,
//! local paths are read from the project's host (over SSH with `--remote`).

use super::{Context, FeatureMeta};
use crate::tools::OciFetcher;
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
        ctx.host
            .fetch_dir(&ctx.base_dir.join(reference), work)
            .with_context(|| format!("reading local feature {reference}"))?
    } else {
        pull(ctx.oci, reference, work)?
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

/// The fetcher drops the feature's layer (a plain tar, per the features spec)
/// into `work`; unpack it and return the directory holding `install.sh`.
fn pull(oci: &dyn OciFetcher, reference: &str, work: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(work)?;
    oci.pull(reference, None, work)
        .with_context(|| format!("pulling feature {reference}"))?;

    let files: Vec<PathBuf> = std::fs::read_dir(work)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    // The features spec has one layer; another shape is not guessed at.
    let [layer] = files.as_slice() else {
        bail!(
            "{reference} has {} layers, a feature has exactly one",
            files.len()
        );
    };
    let content = work.join("content");
    unpack(layer, &content).with_context(|| format!("unpacking feature {reference}"))?;
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
    use crate::tools::Arch;

    /// A registry that holds one feature, as a plain-tar layer.
    struct OneFeature;

    impl OciFetcher for OneFeature {
        fn pull(&self, reference: &str, _arch: Option<Arch>, into: &Path) -> Result<()> {
            assert_eq!(reference, "ghcr.io/x/feat:1");
            let mut layer = tar::Builder::new(Vec::new());
            let mut add = |name: &str, body: &str| {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                layer.append_data(&mut h, name, body.as_bytes()).unwrap();
            };
            add(
                "devcontainer-feature.json",
                r#"{"id": "feat", "version": "1.0.0"}"#,
            );
            add("install.sh", "#!/bin/sh\n");
            std::fs::write(into.join("sha256-abc"), layer.into_inner().unwrap())?;
            Ok(())
        }
    }

    #[test]
    fn a_registry_feature_is_pulled_through_the_fetcher_and_unpacked() {
        let work = crate::tmpfile::TempDir::new("pull-test").unwrap();
        let ctx = Context {
            base_dir: Path::new("/p/.devcontainer"),
            host: &crate::host::fake::FakeHost::new(),
            oci: &OneFeature,
        };
        let (meta, dir) = fetch(&ctx, "ghcr.io/x/feat:1", &work.path().join("0")).unwrap();
        assert_eq!(meta.id, "feat");
        assert!(dir.join("install.sh").is_file());
    }

    /// An artifact with two layers, which is not a feature.
    struct TwoLayers;

    impl OciFetcher for TwoLayers {
        fn pull(&self, _reference: &str, _arch: Option<Arch>, into: &Path) -> Result<()> {
            std::fs::write(into.join("sha256-a"), b"")?;
            std::fs::write(into.join("sha256-b"), b"")?;
            Ok(())
        }
    }

    #[test]
    fn an_artifact_with_more_than_one_layer_is_not_a_feature() {
        let work = crate::tmpfile::TempDir::new("pull-two").unwrap();
        let ctx = Context {
            base_dir: Path::new("/p/.devcontainer"),
            host: &crate::host::fake::FakeHost::new(),
            oci: &TwoLayers,
        };
        let err = fetch(&ctx, "ghcr.io/x/feat:1", &work.path().join("0"))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("2 layers"), "{err:#}");
    }

    #[test]
    fn a_fetcher_that_fails_fails_the_feature_with_its_reference() {
        let work = crate::tmpfile::TempDir::new("pull-fail").unwrap();
        let ctx = Context {
            base_dir: Path::new("/p/.devcontainer"),
            host: &crate::host::fake::FakeHost::new(),
            oci: &crate::tools::NoRegistry,
        };
        let err = fetch(&ctx, "ghcr.io/x/feat:1", &work.path().join("0"))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("ghcr.io/x/feat:1"), "{err:#}");
    }

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
