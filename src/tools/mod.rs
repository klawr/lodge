//! The helper programs lodge runs on this machine to build a bundle or fetch a
//! feature, each behind an interface.
//!
//! What lodge needs from them is small: which libraries a binary needs
//! ([`DepReader`]), relinking a copy of a binary for the bundle's prefix
//! ([`ElfEditor`]), and pulling an OCI artifact ([`OciFetcher`]). Packing and
//! feature fetching take those interfaces, so an implementation is one new type
//! handed in where `cmd/` builds the [`Toolchain`], and a test hands in a fake.
//! The implementations in [`external`] run the programs, started through the
//! `host_tools` registry; [`oci`] pulls from a registry in-process.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod external;
mod oci;

pub use crate::arch::Arch;
pub use external::{Ldd, Patchelf};
pub use oci::{DockerConfig, NativeOci, UreqHttp};

/// What a dynamically linked binary needs.
#[derive(Clone)]
pub struct Deps {
    /// The shared libraries to copy, the dynamic linker among them.
    pub libs: Vec<PathBuf>,
    /// The dynamic linker, `None` for a static binary.
    pub linker: Option<PathBuf>,
}

impl Deps {
    pub fn is_empty(&self) -> bool {
        self.libs.is_empty()
    }
}

/// Which shared libraries a binary needs, and its dynamic linker.
pub trait DepReader {
    /// A static binary has no libraries and no linker. A library that cannot be
    /// found is warned about and left out, not an error.
    fn read(&self, binary: &str) -> Result<Deps>;
}

/// Rewrites binaries so they run from a bundle's prefix.
pub trait ElfEditor {
    /// Sets the dynamic linker of `file` to `interpreter` and its library search
    /// path to `rpath` (as a `DT_RPATH`, which also covers the libraries'
    /// own dependencies), in place.
    fn relink(&self, file: &Path, interpreter: &str, rpath: &str) -> Result<()>;
}

/// Pulls artifacts from an OCI registry. It knows registries, not what an
/// artifact holds.
pub trait OciFetcher {
    /// Pulls the artifact `reference` into the directory `into`, which exists:
    /// each layer ends up there as a file named after its digest, checked
    /// against it. When the reference is an index of images for several
    /// machines, `arch` picks one (for Linux); without it the index must hold
    /// only one image.
    fn pull(&self, reference: &str, arch: Option<Arch>, into: &Path) -> Result<()>;

    /// What `reference` points at right now, named by its content: pulling the
    /// pinned reference gets the same bytes whatever the tag is moved to later,
    /// and the digest changes exactly when the content does. For an index it is
    /// the image for `arch`, so it holds one machine's artifact.
    fn pin(&self, reference: &str, arch: Option<Arch>) -> Result<Pinned>;
}

/// What is published to a registry: one layer, which is a file, and the types
/// that say what the artifact is. The pusher knows registries, not what the
/// layer holds.
pub struct Artifact<'a> {
    /// Says what kind of artifact this is, to anything that lists a registry.
    pub artifact_type: &'a str,
    pub layer_media_type: &'a str,
    pub layer: &'a Path,
}

/// Publishes artifacts to an OCI registry. Separate from [`OciFetcher`], so a
/// machine that only reads never needs a way to write, and a fake of one does not
/// have to implement the other.
pub trait OciPusher {
    /// Publishes `artifact` as `reference`, which names a tag: what that tag held
    /// is replaced (last write wins). Returns what was written, by digest.
    fn push(&self, reference: &str, artifact: &Artifact) -> Result<Pinned>;
}

/// A reference and the content it named when it was pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// `registry/repository@sha256:<hex>`: `pull` takes it as any reference.
    pub reference: String,
    /// `sha256:<hex>`.
    pub digest: String,
}

/// The tools lodge uses on this machine. Built once, in `cmd/`.
pub struct Toolchain {
    pub deps: Box<dyn DepReader>,
    pub elf: Box<dyn ElfEditor>,
    pub oci: Arc<dyn OciFetcher>,
}

/// A registry that is never reached: for tests that must not pull.
#[cfg(test)]
pub(crate) struct NoRegistry;

#[cfg(test)]
impl OciFetcher for NoRegistry {
    fn pull(&self, reference: &str, _arch: Option<Arch>, _into: &Path) -> Result<()> {
        anyhow::bail!("no registry in this test (pulling {reference})")
    }

    fn pin(&self, reference: &str, _arch: Option<Arch>) -> Result<Pinned> {
        anyhow::bail!("no registry in this test (pinning {reference})")
    }
}

/// A registry that holds what it is given, for tests of what is built on
/// [`OciFetcher`]: each artifact is one layer, under a tag, with a digest.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeRegistry {
    artifacts: std::sync::Mutex<std::collections::BTreeMap<String, (String, Vec<u8>)>>,
    pulled: std::sync::Mutex<Vec<String>>,
    pushes: std::sync::Mutex<Vec<(String, String, String)>>,
}

#[cfg(test)]
impl FakeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `reference` (as asked for, tag and all) now holds `layer`, with `digest`.
    pub fn with(self, reference: &str, digest: &str, layer: &[u8]) -> Self {
        self.artifacts
            .lock()
            .unwrap()
            .insert(reference.into(), (digest.into(), layer.to_vec()));
        self
    }

    /// What was pulled, in order.
    pub fn pulled(&self) -> Vec<String> {
        self.pulled.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl FakeRegistry {
    /// What was pushed: the reference, the artifact type and the layer's type.
    pub fn pushed(&self) -> Vec<(String, String, String)> {
        self.pushes.lock().unwrap().clone()
    }
}

/// A digest that follows the content, which is all a fake needs.
#[cfg(test)]
fn fake_digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{:0>64x}",
        bytes.iter().map(|b| u64::from(*b)).sum::<u64>()
    )
}

#[cfg(test)]
impl OciPusher for FakeRegistry {
    fn push(&self, reference: &str, artifact: &Artifact) -> Result<Pinned> {
        let layer = std::fs::read(artifact.layer)?;
        let digest = fake_digest(&layer);
        self.artifacts
            .lock()
            .unwrap()
            .insert(reference.into(), (digest.clone(), layer));
        self.pushes.lock().unwrap().push((
            reference.into(),
            artifact.artifact_type.into(),
            artifact.layer_media_type.into(),
        ));
        let name = reference.rsplit_once(':').map_or(reference, |(n, _)| n);
        Ok(Pinned {
            reference: format!("{name}@{digest}"),
            digest,
        })
    }
}

#[cfg(test)]
impl OciFetcher for FakeRegistry {
    fn pull(&self, reference: &str, _arch: Option<Arch>, into: &Path) -> Result<()> {
        self.pulled.lock().unwrap().push(reference.into());
        let artifacts = self.artifacts.lock().unwrap();
        // A pinned reference names the digest, a plain one the tag.
        let (digest, layer) = artifacts
            .iter()
            .find(|(name, (digest, _))| {
                name.as_str() == reference || reference == format!("{name}@{digest}")
            })
            .map(|(_, v)| v)
            .ok_or_else(|| anyhow::anyhow!("not found: {reference}"))?;
        std::fs::write(into.join(digest.replace(':', "-")), layer)?;
        Ok(())
    }

    fn pin(&self, reference: &str, _arch: Option<Arch>) -> Result<Pinned> {
        let artifacts = self.artifacts.lock().unwrap();
        let (digest, _) = artifacts
            .get(reference)
            .ok_or_else(|| anyhow::anyhow!("not found: {reference}"))?;
        Ok(Pinned {
            reference: format!("{reference}@{digest}"),
            digest: digest.clone(),
        })
    }
}

/// The external-program tools, for tests of what is built on them.
#[cfg(test)]
pub(crate) fn external() -> Toolchain {
    Toolchain {
        deps: Box::new(Ldd),
        elf: Box::new(Patchelf),
        oci: Arc::new(NoRegistry),
    }
}
