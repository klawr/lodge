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
    /// the image for `arch`, so it holds one machine's artifact. A registry that
    /// cannot be reached at all is an error that [`Unreachable`] marks.
    fn pin(&self, reference: &str, arch: Option<Arch>) -> Result<Pinned>;
}

/// What publishing does when something is already published where it is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// Take its place.
    Yes,
    /// Refuse, and leave it as it is.
    No,
}

/// What is published to a registry: one layer, which is a file, for one machine,
/// and the types that say what the artifact is. The pusher knows registries, not what the
/// layer holds.
pub struct Artifact<'a> {
    /// Says what kind of artifact this is, to anything that lists a registry.
    pub artifact_type: &'a str,
    pub layer_media_type: &'a str,
    pub layer: &'a Path,
    /// The machine it is for (Linux on this CPU). A tag holds one image per
    /// machine, so publishing for another one keeps what is there.
    pub arch: Arch,
    /// What to do when the tag already holds an image for this machine.
    pub replace: Replace,
}

/// Publishes artifacts to an OCI registry. Separate from [`OciFetcher`], so a
/// machine that only reads never needs a way to write, and a fake of one does not
/// have to implement the other.
pub trait OciPusher {
    /// Publishes `artifact` under `reference`, which names a tag. The tag holds an
    /// index with one image per architecture: this architecture's entry is
    /// replaced (last write wins, unless the artifact says not to) and the others
    /// are kept. Returns the image that was written, by digest.
    fn push(&self, reference: &str, artifact: &Artifact) -> Result<Pinned>;
}

/// Marks an error as the registry not having been reached at all (no connection,
/// no answer), as opposed to it refusing, or not having what was asked for. A
/// caller that already holds the content may carry on without the registry.
#[derive(Debug)]
pub struct Unreachable;

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the registry could not be reached")
    }
}

impl std::error::Error for Unreachable {}

/// A reference and the content it named when it was pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// `registry/repository@sha256:<hex>`: `pull` takes it as any reference.
    pub reference: String,
    /// `sha256:<hex>`.
    pub digest: String,
}

/// What building a bundle takes. Only a machine that can run the programs has
/// it: see [`Toolchain::packer`].
pub struct Packer {
    pub deps: Box<dyn DepReader>,
    pub elf: Box<dyn ElfEditor>,
}

/// Why a machine that has no [`Packer`] cannot build a bundle, and what to do
/// instead.
const CANNOT_PACK: &str = "this machine cannot build a bundle (that takes ldd and patchelf, \
so Linux); use one built elsewhere: `--bundle FILE`, or `bundle = \"…\"` in the config \
(a file or an `oci://` reference)";

/// The tools lodge uses on this machine. Built once, in `cmd/`.
pub struct Toolchain {
    /// `None` on a machine that only uses bundles other machines built.
    pub packer: Option<Packer>,
    pub oci: Arc<dyn OciFetcher>,
}

impl Toolchain {
    /// What building a bundle uses, or an error that says this machine cannot.
    pub fn packer(&self) -> Result<&Packer> {
        self.packer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(CANNOT_PACK))
    }
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
/// [`OciFetcher`]: each tag holds one layer per architecture (or one for any,
/// when it was put there by [`FakeRegistry::with`]), each with a digest.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeRegistry {
    tags: std::sync::Mutex<std::collections::BTreeMap<String, Images>>,
    pulled: std::sync::Mutex<Vec<String>>,
    pushes: std::sync::Mutex<Vec<(String, String, String, Arch)>>,
}

/// The images under a tag: by architecture, `None` for one that is for any.
#[cfg(test)]
type Images = std::collections::BTreeMap<Option<String>, (String, Vec<u8>)>;

#[cfg(test)]
impl FakeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `reference` (as asked for, tag and all) now holds `layer`, with `digest`,
    /// for any architecture.
    pub fn with(self, reference: &str, digest: &str, layer: &[u8]) -> Self {
        self.tags
            .lock()
            .unwrap()
            .entry(reference.into())
            .or_default()
            .insert(None, (digest.into(), layer.to_vec()));
        self
    }

    /// What was pulled, in order.
    pub fn pulled(&self) -> Vec<String> {
        self.pulled.lock().unwrap().clone()
    }

    /// What was pushed: the reference, the artifact type, the layer's type and
    /// the architecture.
    pub fn pushed(&self) -> Vec<(String, String, String, Arch)> {
        self.pushes.lock().unwrap().clone()
    }

    /// The architectures a tag holds an image for.
    pub fn architectures(&self, reference: &str) -> Vec<String> {
        self.tags
            .lock()
            .unwrap()
            .get(reference)
            .map(|images| images.keys().flatten().cloned().collect())
            .unwrap_or_default()
    }

    /// The image of `reference` for `arch`, as a real index answers: the entry for
    /// it, or the only one when none is asked for.
    fn image(&self, reference: &str, arch: Option<Arch>) -> Result<(String, Vec<u8>)> {
        let tags = self.tags.lock().unwrap();
        let images = tags
            .get(reference)
            .ok_or_else(|| anyhow::anyhow!("not found: {reference}"))?;
        let found = match arch {
            Some(arch) => images
                .get(&Some(arch.to_string()))
                .or_else(|| images.get(&None)),
            None if images.len() == 1 => images.values().next(),
            None => None,
        };
        found
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no image of {reference} for {arch:?}"))
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
        let held = self
            .tags
            .lock()
            .unwrap()
            .get(reference)
            .is_some_and(|images| images.contains_key(&Some(artifact.arch.to_string())));
        if held && artifact.replace == Replace::No {
            anyhow::bail!("{reference} already holds an image for {}", artifact.arch);
        }
        self.tags
            .lock()
            .unwrap()
            .entry(reference.into())
            .or_default()
            .insert(Some(artifact.arch.to_string()), (digest.clone(), layer));
        self.pushes.lock().unwrap().push((
            reference.into(),
            artifact.artifact_type.into(),
            artifact.layer_media_type.into(),
            artifact.arch,
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
    fn pull(&self, reference: &str, arch: Option<Arch>, into: &Path) -> Result<()> {
        self.pulled.lock().unwrap().push(reference.into());
        // A pinned reference names the digest, a plain one the tag.
        let found = {
            let tags = self.tags.lock().unwrap();
            tags.iter()
                .flat_map(|(name, images)| images.values().map(move |v| (name, v)))
                .find(|(name, (digest, _))| reference == format!("{name}@{digest}"))
                .map(|(_, v)| v.clone())
        };
        let (digest, layer) = match found {
            Some(found) => found,
            None => self.image(reference, arch)?,
        };
        std::fs::write(into.join(digest.replace(':', "-")), layer)?;
        Ok(())
    }

    fn pin(&self, reference: &str, arch: Option<Arch>) -> Result<Pinned> {
        let (digest, _) = self.image(reference, arch)?;
        Ok(Pinned {
            reference: format!("{reference}@{digest}"),
            digest,
        })
    }
}

/// The external-program tools, for tests of what is built on them.
#[cfg(test)]
pub(crate) fn external() -> Toolchain {
    Toolchain {
        packer: Some(Packer {
            deps: Box::new(Ldd),
            elf: Box::new(Patchelf),
        }),
        oci: Arc::new(NoRegistry),
    }
}

/// A machine that cannot build a bundle: the tools of [`external`] without the packer.
#[cfg(test)]
pub(crate) fn consumer_only() -> Toolchain {
    Toolchain {
        packer: None,
        ..external()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_without_a_packer_says_what_to_do_instead() {
        let err = consumer_only().packer().err().unwrap().to_string();
        assert!(err.contains("cannot build a bundle"), "{err}");
        assert!(err.contains("--bundle") && err.contains("oci://"), "{err}");
        assert!(external().packer().is_ok());
    }
}
