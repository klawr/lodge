//! `ldg inject add|rm|ls`: change what is injected without opening the config.
//! The editing itself, and the check that the result is a valid config, is
//! [`crate::config::edit`]; this only turns arguments into an entry.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use super::Ctx;
use super::add::add_binary;
use crate::cli::{AddCommand, InjectCommand, RemoveCommand};
use crate::config::edit::{self, Kind, NewBinary, NewEntry};
use crate::config::expand_tilde;
use crate::layout::tilde;
use crate::output::{info, line, note, warning};

/// The home directory of the user running lodge, which paths under it are relative to.
fn home_of(ctx: &Ctx) -> PathBuf {
    ctx.local
        .var("HOME")
        .map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub fn run(ctx: &Ctx, command: Option<InjectCommand>) -> Result<()> {
    let path = &ctx.config_path;
    let home = home_of(ctx);
    match command.unwrap_or(InjectCommand::Ls) {
        InjectCommand::Ls => return list(ctx, &home),
        InjectCommand::Add { what } => match what {
            AddCommand::Binary {
                name,
                path: given,
                target,
                no_deps,
                no_data,
            } => {
                // A bare name is a program on PATH, the way `ldg init` finds tools.
                let given = match given {
                    Some(p) => p,
                    None => crate::shell::which(&name).ok_or_else(|| {
                        anyhow::anyhow!("`{name}` is not on PATH; give its location after the name")
                    })?,
                };
                let entry = NewEntry::Binary(NewBinary {
                    name: name.clone(),
                    path: host_path(given, &home)?,
                    target,
                    copy_deps: !no_deps,
                });
                for placed in add_binary(path, entry, &home, !no_data)? {
                    info!("added data `{placed}` for `{name}` (it sits beside the program)");
                }
            }
            AddCommand::Data {
                path: given,
                target,
                binary,
            } => {
                let entry = NewEntry::data(host_path(given, &home)?, target, binary);
                add(path, entry)?
            }
        },
        InjectCommand::Rm { what } => {
            let (kind, key) = match what {
                RemoveCommand::Binary { name } => (Kind::Binary, name),
                RemoveCommand::Data { path: given } => (Kind::Data, data_key(path, given)?),
            };
            edit::remove(path, kind, &key)?;
            info!("removed {} `{key}` from {}", kind.key(), path.display());
        }
    }
    bundle_note(ctx);
    Ok(())
}

fn add(path: &Path, entry: NewEntry) -> Result<()> {
    edit::add(path, &entry)?;
    info!(
        "added {} `{}` to {}",
        entry.kind().key(),
        entry.key(),
        path.display()
    );
    Ok(())
}

/// The key of the data in the config at `path` that comes from `given`: the entry
/// is found by the place it means, however the config writes it (`~/x` or the
/// whole path).
fn data_key(path: &Path, given: PathBuf) -> Result<String> {
    let want = std::path::absolute(expand_tilde(given.clone())).unwrap_or(given);
    edit::list(path, Kind::Data)?
        .into_iter()
        .filter_map(|d| d.path)
        .find(|written| expand_tilde(PathBuf::from(written)) == want)
        .ok_or_else(|| anyhow::anyhow!("no [[data]] `{}` in {}", want.display(), path.display()))
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

fn list(ctx: &Ctx, home: &Path) -> Result<()> {
    let config = ctx.config()?;
    if config.binaries.is_empty() && config.data.is_empty() {
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
        .max()
        .unwrap_or(0);
    for item in &config.binaries {
        line!("binary  {:<width$}  {}", item.name, item.path.display());
    }
    for item in &config.data {
        let of = match &item.binary {
            Some(binary) => format!(" · for {binary}"),
            None => String::new(),
        };
        line!("data    {} → {}{of}", item.path.display(), item.dest(home));
    }
    Ok(())
}
