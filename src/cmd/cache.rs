//! Bundle commands: `pack`, `bundle inspect|verify`, `cache [list|clear]`.

use anyhow::Result;
use std::path::PathBuf;

use super::{Ctx, ago, human_size};
use crate::bundle;
use crate::cli::{BundleCommand, CacheCommand};
use crate::output::{info, line, note};

pub fn pack(ctx: &Ctx, output: Option<PathBuf>) -> Result<()> {
    let config = ctx.config()?;
    let output = output.unwrap_or_else(|| PathBuf::from(bundle::default_file_name()));
    let manifest = bundle::pack::pack_to_file(&config, &output)?;
    info!(
        "packed {} tool(s) and {} librar{} for {} into {} (id {})",
        manifest.tools.len(),
        manifest.libs.len(),
        if manifest.libs.len() == 1 { "y" } else { "ies" },
        manifest.arch,
        output.display(),
        manifest.id()
    );
    Ok(())
}

pub fn bundle(command: BundleCommand) -> Result<()> {
    match command {
        BundleCommand::Inspect { file } => bundle::inspect(&file),
        BundleCommand::Verify { file } => bundle::verify_file(&file),
    }
}

pub fn cache(command: Option<CacheCommand>) -> Result<()> {
    let root = bundle::source::cache_root()?;
    match command.unwrap_or(CacheCommand::List) {
        CacheCommand::List => {
            let entries = bundle::source::cache_entries(&root);
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
                bundle::source::KEEP
            );
        }
        CacheCommand::Clear => {
            let (n, size) = bundle::source::clear_cache(&root);
            info!(
                "removed {n} cached bundle(s), {}; the next `ldg up` packs again",
                human_size(size)
            );
        }
    }
    Ok(())
}
