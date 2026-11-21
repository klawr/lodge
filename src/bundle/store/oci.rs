//! A bundle kept in an OCI registry: `oci://ghcr.io/me/lodge:1`.
//!
//! The artifact is an image, or an index with one image per machine, whose single
//! layer is the bundle's tar. lodge does not read the layer's media type: what
//! decides that it is a bundle is the manifest inside the tar, which
//! extraction checks.

use super::{BundleStore, Identity};
use crate::arch::Arch;
use crate::output::info;
use crate::tools::OciFetcher;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What marks a reference as this store's. The only place that spells it.
const SCHEME: &str = "oci://";

pub struct OciStore {
    oci: Arc<dyn OciFetcher>,
}

impl OciStore {
    pub fn new(oci: Arc<dyn OciFetcher>) -> Self {
        Self { oci }
    }
}

impl BundleStore for OciStore {
    fn form(&self) -> &'static str {
        "oci://<registry>/<name>[:<tag>|@<digest>]"
    }

    fn handles(&self, reference: &str) -> bool {
        reference.starts_with(SCHEME)
    }

    /// The key is the digest of the image for this machine, which the registry
    /// answers with one request: an unchanged tag is not downloaded again.
    fn identify(&self, reference: &str, arch: Option<Arch>) -> Result<Identity> {
        let name = reference.strip_prefix(SCHEME).unwrap_or(reference);
        let pinned = self
            .oci
            .pin(name, arch)
            .with_context(|| format!("looking up bundle {reference}"))?;
        let hex = pinned
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&pinned.digest);
        Ok(Identity {
            key: format!("o-{hex}"),
            pinned: pinned.reference,
        })
    }

    fn fetch(&self, identity: &Identity, into: &Path) -> Result<PathBuf> {
        info!("pulling {}", identity.pinned);
        self.oci
            .pull(&identity.pinned, None, into)
            .with_context(|| format!("pulling bundle {}", identity.pinned))?;
        let mut layers: Vec<PathBuf> = std::fs::read_dir(into)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect();
        // A bundle is one tar; another shape is not guessed at.
        match (layers.pop(), layers.is_empty()) {
            (Some(layer), true) => Ok(layer),
            (Some(_), false) => bail!(
                "{} has more than one layer, a bundle has exactly one",
                identity.pinned
            ),
            (None, _) => bail!("{} has no layer", identity.pinned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;
    use crate::tools::FakeRegistry;

    const DIGEST: &str = "sha256:aaaa";

    fn store(registry: FakeRegistry) -> OciStore {
        OciStore::new(Arc::new(registry))
    }

    #[test]
    fn only_the_oci_scheme_is_its_own() {
        let s = store(FakeRegistry::new());
        assert!(s.handles("oci://ghcr.io/me/lodge:1"));
        assert!(!s.handles("ghcr.io/me/lodge:1"));
        assert!(!s.handles("./lodge.tar"));
    }

    #[test]
    fn a_tag_is_identified_by_the_digest_it_points_at() {
        let s = store(FakeRegistry::new().with("ghcr.io/me/lodge:1", DIGEST, b"tar"));
        let id = s.identify("oci://ghcr.io/me/lodge:1", None).unwrap();
        assert_eq!(id.key, "o-aaaa");
        assert_eq!(id.pinned, format!("ghcr.io/me/lodge:1@{DIGEST}"));
    }

    #[test]
    fn the_pinned_reference_is_fetched_as_the_tar() {
        let s = store(FakeRegistry::new().with("ghcr.io/me/lodge:1", DIGEST, b"the tar"));
        let id = s.identify("oci://ghcr.io/me/lodge:1", None).unwrap();
        let dir = TempDir::new("oci-store").unwrap();
        let tar = s.fetch(&id, dir.path()).unwrap();
        assert_eq!(std::fs::read(tar).unwrap(), b"the tar");
    }

    #[test]
    fn a_lookup_that_fails_names_the_bundle() {
        let err = store(FakeRegistry::new())
            .identify("oci://ghcr.io/me/none:1", None)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("oci://ghcr.io/me/none:1"),
            "{err:#}"
        );
    }

    #[test]
    fn more_than_one_layer_is_not_a_bundle() {
        struct TwoLayers;
        impl OciFetcher for TwoLayers {
            fn pull(&self, _r: &str, _a: Option<Arch>, into: &Path) -> Result<()> {
                std::fs::write(into.join("sha256-a"), b"")?;
                std::fs::write(into.join("sha256-b"), b"")?;
                Ok(())
            }
            fn pin(&self, r: &str, _a: Option<Arch>) -> Result<crate::tools::Pinned> {
                bail!("not needed ({r})")
            }
        }
        let id = Identity {
            key: "o-x".into(),
            pinned: "ghcr.io/me/lodge@sha256:x".into(),
        };
        let dir = TempDir::new("oci-store").unwrap();
        let err = OciStore::new(Arc::new(TwoLayers))
            .fetch(&id, dir.path())
            .unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
    }
}
