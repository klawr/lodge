//! Bundles: a portable snapshot of the user's environment.
//!
//! A bundle is a plain tar holding the user's tools already relinked to a bundled
//! glibc, their libraries, their configs, and a manifest. Building one (`pack`)
//! is the only step that needs `patchelf` or reads the host's own binaries;
//! installing from one needs neither it nor `ldd`, so a bundle made on a Linux
//! machine can be used from any machine that can reach a container.
//!
//! - [`manifest`]: what a bundle contains, and the rules it must obey
//! - [`archive`]: writing, reading and verifying the tar
//! - [`pack`]: building a bundle from the user's config (`deps` remembers what a `DepReader` said,
//!   `elf` reads a binary's architecture)
//! - [`arch`]: CPU architectures, as bundles, binaries and machines name them
//! - [`source`]: readying a bundle directory to inject from
//! - [`cache`]: where readied bundle directories are kept
//! - `hash`: the content hash of a file or tree, used throughout

pub mod arch;
pub mod archive;
pub mod cache;
pub mod deps;
pub mod elf;
mod hash;
pub mod manifest;
pub mod pack;
pub mod source;

use anyhow::Result;
use std::path::Path;

use manifest::Manifest;

/// The file name `ldg pack` uses when none is given: `lodge-<arch>.tar`, with
/// the architecture named as the manifest names it.
pub fn default_file_name() -> Result<String> {
    Ok(format!("lodge-{}.tar", arch::Arch::host()?))
}

/// Checks that the bundle at `file` is intact: every file the manifest lists is
/// present and matches its recorded hash. Run after carrying a bundle somewhere.
pub fn verify_file(file: &Path) -> Result<Manifest> {
    let scratch = crate::tmpfile::TempDir::new("verify")?;
    archive::extract(file, scratch.path())
}
