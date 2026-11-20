//! Bundles: a portable snapshot of the user's environment.
//!
//! A bundle is a plain tar holding the user's tools already relinked to a bundled
//! glibc, their libraries, their configs, and a manifest. Building one (`pack`)
//! is the only step that needs `ldd` and `patchelf` or reads the host's own
//! binaries; installing from one needs neither, so a bundle made on a Linux
//! machine can be used from any machine that can reach a container.
//!
//! - [`manifest`]: what a bundle contains, and the rules it must obey
//! - [`archive`]: writing, reading and verifying the tar
//! - [`pack`]: building a bundle from the user's config
//! - [`source`]: readying a bundle directory to inject from, with a cache
//! - `tree`: the content hash used throughout

pub mod archive;
pub mod manifest;
pub mod pack;
pub mod source;
mod tree;

use anyhow::{Context, Result};
use std::path::Path;

use crate::output::line;

// The container-side layout is part of the bundle format: relinked binaries bake
// these paths in (as their interpreter and library search path).
pub const LODGE_LIB: &str = "/opt/lodge/lib";
pub const LODGE_BIN: &str = "/opt/lodge/bin";
// The dynamic linker is placed here so a binary whose interpreter was set to
// `/lodge/<ld>` finds it when run directly. The prefix is 7 bytes, matching
// `/lib64/`, so the interpreter can be patched in place.
pub const LODGE_DIR: &str = "/lodge";

/// The file name `ldg pack` uses when none is given: `lodge-<arch>.tar`.
pub fn default_file_name() -> String {
    format!("lodge-{}.tar", std::env::consts::ARCH)
}

/// Prints what is in the bundle at `file`.
pub fn inspect(file: &Path) -> Result<()> {
    let manifest = archive::read_manifest(file)?;
    let size = std::fs::metadata(file)
        .with_context(|| format!("reading {}", file.display()))?
        .len();

    line!("bundle    {} ({})", file.display(), human_size(size));
    line!("id        {}", manifest.id());
    line!(
        "format    {} · {} · packed by ldg {}",
        manifest.format,
        manifest.arch,
        manifest.lodge_version
    );
    for tool in &manifest.tools {
        let mut parts = Vec::new();
        if let Some(b) = &tool.binary {
            let kind = if b.dynamic { "relinked" } else { "as is" };
            parts.push(format!("bin → {} ({kind})", b.target));
        }
        if let Some(c) = &tool.config {
            let kind = if c.is_dir { "dir" } else { "file" };
            parts.push(format!("config → {} ({kind})", c.target));
        }
        line!("tool      {:<12} {}", tool.name, parts.join(" · "));
    }
    let linker = manifest.linker.as_deref().unwrap_or("none");
    line!("libs      {} (linker {linker})", manifest.libs.len());
    line!(
        "settings  {} alias(es) · git safe.directory: {}",
        manifest.aliases.len(),
        manifest.git_safe_directories.join(", ")
    );
    Ok(())
}

/// Checks that the bundle at `file` is intact: every file the manifest lists is
/// present and matches its recorded hash. Run after carrying a bundle somewhere.
pub fn verify_file(file: &Path) -> Result<()> {
    let scratch = crate::tmpfile::TempDir::new("verify")?;
    let manifest = archive::extract(file, scratch.path())?;
    line!(
        "ok  {} tool(s), {} librar{}, id {}",
        manifest.tools.len(),
        manifest.libs.len(),
        if manifest.libs.len() == 1 { "y" } else { "ies" },
        manifest.id()
    );
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
