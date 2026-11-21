//! Bundle commands: `pack`, `bundle inspect|verify`, `cache [list|clear]`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::{Ctx, ago, human_size};
use crate::arch::Arch;
use crate::bundle;
use crate::bundle::store::{Identity, Replace};
use crate::cli::{BundleCommand, CacheCommand};
use crate::output::{info, line, note, plural};
use crate::tmpfile::TempDir;

pub fn pack(ctx: &Ctx, output: Option<String>, replace: Replace) -> Result<()> {
    let config = ctx.config()?;
    let output = match output {
        Some(o) => o,
        None => bundle::default_file_name()?,
    };
    // A destination nobody writes to fails before anything is packed.
    let sink = ctx.sinks.find(&output)?;
    // The tar is built where the sink can place it cheapest: for a file, beside it,
    // so a large bundle is not held in the scratch space as well.
    let scratch = match sink.staging(&output) {
        Some(dir) => TempDir::new_in(&dir, "pack-out"),
        None => TempDir::new("pack-out"),
    }?;
    let tar = scratch.path().join("bundle.tar");
    let manifest = bundle::pack::pack_to_file(&config, &tar, &ctx.tools)?;
    let written = sink.put(&output, &tar, manifest.arch()?, replace)?;
    info!(
        "packed {} and {} for {} into {} (id {})",
        plural(manifest.tools.len(), "tool", "tools"),
        plural(manifest.libs.len(), "library", "libraries"),
        manifest.arch,
        written,
        manifest.id()
    );
    Ok(())
}

/// The tar of the bundle `reference` names, from whichever store owns it: always
/// fetched (a check must look at the bytes, not at what the cache remembers) and
/// kept only as long as the returned directory lives.
fn fetch(ctx: &Ctx, reference: &str) -> Result<(TempDir, PathBuf, Identity)> {
    let store = ctx.bundles.find(reference)?;
    let identity = store.identify(reference, Arch::host().ok())?;
    let scratch = TempDir::new("bundle-fetch")?;
    let tar = store.fetch(&identity, scratch.path())?;
    Ok((scratch, tar, identity))
}

pub fn bundle(ctx: &Ctx, command: BundleCommand) -> Result<()> {
    match command {
        BundleCommand::Inspect { reference } => {
            let (_scratch, tar, identity) = fetch(ctx, &reference)?;
            inspect(&reference, &identity.pinned, &tar)
        }
        BundleCommand::Verify { reference } => {
            let (_scratch, tar, _) = fetch(ctx, &reference)?;
            let manifest = bundle::verify_file(&tar)?;
            line!(
                "ok  {}, {}, id {}",
                plural(manifest.tools.len(), "tool", "tools"),
                plural(manifest.libs.len(), "library", "libraries"),
                manifest.id()
            );
            Ok(())
        }
    }
}

/// Prints what is in the bundle `reference` names, whose tar is at `file`;
/// `pinned` is the reference made exact, shown when it says more.
fn inspect(reference: &str, pinned: &str, file: &Path) -> Result<()> {
    let manifest = bundle::archive::read_manifest(file)?;
    let size = std::fs::metadata(file)
        .with_context(|| format!("reading {}", file.display()))?
        .len();

    line!("bundle    {reference} ({})", human_size(size));
    if pinned != reference {
        line!("source    {pinned}");
    }
    line!("id        {}", manifest.id());
    line!(
        "packed    {} by ldg {}",
        manifest.arch,
        manifest.lodge_version
    );
    for tool in &manifest.tools {
        let mut parts = Vec::new();
        if let Some(b) = &tool.binary {
            let kind = if b.dynamic { "relinked" } else { "as is" };
            match &b.dest {
                Some(dest) => parts.push(format!("bin ({kind}) + wrapper → {dest}")),
                None => parts.push(format!("bin ({kind})")),
            }
        }
        if let Some(c) = &tool.config {
            let kind = if c.is_dir { "dir" } else { "file" };
            parts.push(format!("config → {} ({kind})", c.dest));
        }
        line!("tool      {:<12} {}", tool.name, parts.join(" · "));
    }
    let linker = manifest.linker.as_deref().unwrap_or("none");
    line!("libs      {} (linker {linker})", manifest.libs.len());
    line!(
        "settings  git safe.directory: {}",
        manifest.git_safe_directories.join(", ")
    );
    Ok(())
}

pub fn cache(command: Option<CacheCommand>) -> Result<()> {
    let root = bundle::cache::root()?;
    match command.unwrap_or(CacheCommand::List) {
        CacheCommand::List => {
            let entries = bundle::cache::entries(&root);
            if entries.is_empty() {
                note!("the bundle cache ({}) is empty", root.display());
                return Ok(());
            }
            line!(
                "{:<18} {:<8} {:>5} {:>9}  USED",
                "ID",
                "ARCH",
                "TOOLS",
                "SIZE"
            );
            for e in &entries {
                let (id, arch, tools) = match &e.manifest {
                    Some(m) => (m.id(), m.arch.to_string(), m.tools.len().to_string()),
                    None => ("(incomplete)".into(), "-".into(), "-".into()),
                };
                line!(
                    "{id:<18} {arch:<8} {tools:>5} {:>9}  {}",
                    human_size(e.size),
                    e.modified
                        .map(|m| format!("{} ago", ago(m)))
                        .unwrap_or_default()
                );
            }
            let total: u64 = entries.iter().map(|e| e.size).sum();
            note!(
                "{} in {} (the {} most recently used are kept; `ldg cache clear` empties it)",
                human_size(total),
                root.display(),
                bundle::cache::KEEP
            );
        }
        CacheCommand::Clear => {
            let (n, size) = bundle::cache::clear(&root);
            info!(
                "removed {}, {}; the next `ldg up` packs again",
                plural(n, "cached bundle", "cached bundles"),
                human_size(size)
            );
        }
    }
    Ok(())
}
