//! Bundles: a portable snapshot of the user's environment.
//!
//! A bundle is a plain tar holding the user's tools already relinked to a bundled
//! glibc, their libraries, their configs, and a manifest. Building one (`pack`)
//! is the only step that needs `patchelf` or reads the host's own binaries;
//! installing from one needs neither it nor `ldd`, so a bundle made on a Linux
//! machine can be used from any machine that can reach a container.
//!
//! - [`manifest`]: what a bundle contains, and the rules it must obey
//! - [`archive`]: writing the tar
//! - [`reader`]: reading and verifying one in place, as a [`Snapshot`](crate::snapshot::Snapshot)
//! - [`pack`]: building a bundle from the user's config (`deps` remembers what a `DepReader` said,
//!   `elf` reads a binary's architecture)
//! - [`source`]: readying a bundle directory to inject from
//! - [`cache`]: where readied bundle directories are kept
//! - `hash`: the content hash of a file or tree, used throughout

pub mod archive;
pub mod cache;
pub mod choice;
pub mod deps;
pub mod elf;
mod hash;
pub mod manifest;
pub mod pack;
pub mod reader;
pub mod source;
pub mod store;
#[cfg(test)]
pub(crate) mod testing;

use anyhow::Result;

/// The file name `ldg pack` uses when none is given: `lodge-<arch>.tar`, with
/// the architecture named as the manifest names it.
pub fn default_file_name() -> Result<String> {
    Ok(format!("lodge-{}.tar", crate::arch::Arch::host()?))
}
