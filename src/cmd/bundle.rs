//! Bundle commands: `pack`, `bundle inspect|verify`, `cache [list|clear]`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::{Ctx, ago, human_size};
use crate::bundle;
use crate::cli::{BundleCommand, CacheCommand};
use crate::output::{info, line, note, plural};

pub fn pack(ctx: &Ctx, output: Option<PathBuf>) -> Result<()> {
    let config = ctx.config()?;
    let output = match output {
        Some(o) => o,
        None => PathBuf::from(bundle::default_file_name()?),
    };
    let manifest = bundle::pack::pack_to_file(&config, &output)?;
    info!(
        "packed {} and {} for {} into {} (id {})",
        plural(manifest.tools.len(), "tool", "tools"),
        plural(manifest.libs.len(), "library", "libraries"),
        manifest.arch,
        output.display(),
        manifest.id()
    );
    Ok(())
}

pub fn bundle(command: BundleCommand) -> Result<()> {
    match command {
        BundleCommand::Inspect { file } => inspect(&file),
        BundleCommand::Verify { file } => {
            let manifest = bundle::verify_file(&file)?;
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

/// Prints what is in the bundle at `file`.
fn inspect(file: &Path) -> Result<()> {
    let manifest = bundle::archive::read_manifest(file)?;
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
            parts.push(format!("bin → {} ({kind})", b.dest));
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
        "settings  {} · git safe.directory: {}",
        plural(manifest.aliases.len(), "alias", "aliases"),
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
