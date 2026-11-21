//! The helper programs lodge runs on this machine to build a bundle or fetch a
//! feature, each behind an interface.
//!
//! What lodge needs from them is small: which libraries a binary needs
//! ([`DepReader`]), relinking a copy of a binary for the bundle's prefix
//! ([`ElfEditor`]), and pulling an OCI artifact ([`OciFetcher`]). Packing and
//! feature fetching take those interfaces, so a native implementation (one that
//! needs no `patchelf` or `oras`) is one new type handed in where `cmd/` builds
//! the [`Toolchain`], and a test hands in a fake. The implementations in
//! [`external`] run the programs, started through the `host_tools` registry.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod external;

pub use external::{Ldd, Oras, Patchelf};

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

/// Pulls artifacts from an OCI registry.
pub trait OciFetcher {
    /// Whether this can pull at all: an error saying what is missing if not.
    fn check(&self) -> Result<()>;

    /// Pulls the artifact `reference` into the directory `into`, which exists:
    /// its layer ends up there as a file.
    fn pull(&self, reference: &str, into: &Path) -> Result<()>;
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
    fn check(&self) -> Result<()> {
        anyhow::bail!("no registry in this test")
    }

    fn pull(&self, reference: &str, _into: &Path) -> Result<()> {
        anyhow::bail!("no registry in this test (pulling {reference})")
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
