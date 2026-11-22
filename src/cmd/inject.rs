//! `ldg inject add|rm|ls`: change what is injected without opening the config.
//! The editing itself, and the check that the result is a valid config, is
//! [`crate::config::edit`]; this only turns arguments into an entry.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use super::Ctx;
use super::init::tilde;
use crate::cli::{AddCommand, InjectCommand, RemoveCommand};
use crate::config::edit::{self, Kind, NewEntry};
use crate::config::expand_tilde;
use crate::output::{info, line, note, warning};

pub fn run(ctx: &Ctx, command: Option<InjectCommand>) -> Result<()> {
    let path = &ctx.config_path;
    let home = ctx
        .local
        .var("HOME")
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    match command.unwrap_or(InjectCommand::Ls) {
        InjectCommand::Ls => return list(ctx),
        InjectCommand::Add { what } => match what {
            AddCommand::Binary {
                name,
                path: given,
                target,
                no_deps,
            } => {
                // A bare name is a program on PATH, the way `ldg init` finds tools.
                let given = match given {
                    Some(p) => p,
                    None => crate::shell::which(&name).ok_or_else(|| {
                        anyhow::anyhow!("`{name}` is not on PATH; give its location after the name")
                    })?,
                };
                let mut entry = NewEntry::binary(&name, host_path(given, &home)?);
                entry.target = target;
                entry.copy_deps = !no_deps;
                add(path, entry)?
            }
            AddCommand::Config {
                name,
                path: given,
                target,
            } => {
                let entry = NewEntry::config(&name, host_path(given, &home)?, target);
                add(path, entry)?
            }
        },
        InjectCommand::Rm { what } => {
            let (kind, name) = match what {
                RemoveCommand::Binary { name } => (Kind::Binary, name),
                RemoveCommand::Config { name } => (Kind::Config, name),
            };
            edit::remove(path, kind, &name)?;
            info!("removed {} `{name}` from {}", kind.key(), path.display());
        }
    }
    bundle_note(ctx);
    Ok(())
}

fn add(path: &Path, entry: NewEntry) -> Result<()> {
    edit::add(path, &entry)?;
    info!(
        "added {} `{}` to {}",
        entry.kind.key(),
        entry.name,
        path.display()
    );
    Ok(())
}

/// A host path as it is written in the config: absolute (so it means the same
/// wherever lodge runs from), `~/…` when under the home directory, and there.
pub(super) fn host_path(path: PathBuf, home: &Path) -> Result<String> {
    let path = expand_tilde(path);
    let path = std::path::absolute(&path).unwrap_or(path);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    Ok(tilde(&path, home))
}

/// A name for an entry made from a path: its last part without leading dots
/// (`~/.bashrc` is `bashrc`), cut to what a name may hold.
pub(super) fn name_from(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().trim_start_matches('.').to_string())
        .unwrap_or_default();
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.+".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A configured bundle is injected instead of the entries, which would leave an
/// edit looking like it did nothing.
fn bundle_note(ctx: &Ctx) {
    if let Ok(config) = ctx.config()
        && let Some(bundle) = config.bundle
    {
        warning!(
            "the config sets `bundle = \"{bundle}\"`, which is injected instead of these \
             entries: pack again (`ldg pack`) or remove that line to use the change"
        );
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    let config = ctx.config()?;
    if config.binaries.is_empty() && config.configs.is_empty() {
        note!(
            "nothing is injected by {} (`ldg inject add binary NAME` adds a program)",
            ctx.config_path.display()
        );
        return Ok(());
    }
    let width = config
        .binaries
        .iter()
        .map(|b| b.name.len())
        .chain(config.configs.iter().map(|c| c.name.len()))
        .max()
        .unwrap_or(0);
    for item in &config.binaries {
        line!("binary  {:<width$}  {}", item.name, item.path.display());
    }
    for item in &config.configs {
        let to = item
            .target
            .clone()
            .unwrap_or_else(|| format!("~/.config/{}", item.name));
        line!(
            "config  {:<width$}  {} → {to}",
            item.name,
            item.path.display()
        );
    }
    Ok(())
}
