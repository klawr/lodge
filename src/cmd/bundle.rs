//! Bundle commands: `pack`, `bundle inspect|verify`, `cache [list|clear]`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::prompt::Choose;
use super::table::{Cell, Table, Tone};
use super::{Ctx, ago, human_size};
use crate::arch::Arch;
use crate::bundle;
use crate::bundle::choice::{self, Offer};
use crate::bundle::reader::Bundle;
use crate::bundle::store::{Identity, Replace, Sinks};
use crate::cli::{BundleCommand, CacheCommand};
use crate::config::edit::{self, Setting};
use crate::output::{info, line, note, plural, warning};
use crate::tmpfile::TempDir;
use crate::tools::Toolchain;

/// Packs for the config screen with the helper programs and the places a bundle
/// can be written, which the composition root made.
pub struct LivePacker {
    pub tools: Toolchain,
    pub sinks: Sinks,
}

impl super::configure::Bundles for LivePacker {
    fn pack(&self, path: &Path, output: Option<&str>, replace: Replace) -> Result<String> {
        let config = crate::config::Config::load(path)?;
        let packed = pack_config(
            &config,
            path,
            &self.tools,
            &self.sinks,
            output.map(str::to_string),
            replace,
        )?;
        Ok(packed.summary)
    }

    fn default_file(&self) -> Result<String> {
        bundle::default_file_name()
    }

    fn exists(&self, reference: &str) -> bool {
        Path::new(reference).is_file()
    }

    fn delete(&self, reference: &str) -> Result<()> {
        anyhow::ensure!(
            !self.is_registry(reference),
            "{reference} is in a registry: delete it there"
        );
        std::fs::remove_file(reference).with_context(|| format!("deleting {reference}"))
    }

    fn can_pack(&self) -> bool {
        self.tools.packer.is_some()
    }

    fn is_registry(&self, reference: &str) -> bool {
        // A registry is whatever writes somewhere other than a plain file does.
        let file = self.sinks.find("bundle.tar").ok().map(|s| s.form());
        self.sinks
            .find(reference)
            .ok()
            .is_some_and(|s| Some(s.form()) != file)
    }
}

pub fn pack(ctx: &Ctx, output: Option<String>, replace: Replace) -> Result<()> {
    let config = ctx.config()?;
    let packed = pack_config(
        &config,
        &ctx.config_path,
        &ctx.tools,
        &ctx.sinks,
        output,
        replace,
    )?;
    info!("{}", packed.summary);
    if let Some(offered) = packed.offered {
        info!("{offered}");
    }
    Ok(())
}

/// What packing made.
pub struct Packed {
    /// What was made and where, in a line.
    pub summary: String,
    /// What was said of the config, when the bundle was put on offer in it.
    pub offered: Option<String>,
}

/// Packs `config`, read from `config_path`, into `output` (a file, or an `oci://`
/// reference to publish to; `lodge-<arch>.tar` here when none is given), and puts
/// it on offer in the config (see [`offer`]). What was made is returned, for the
/// caller to say where it is said.
pub fn pack_config(
    config: &crate::config::Config,
    config_path: &Path,
    tools: &Toolchain,
    sinks: &Sinks,
    output: Option<String>,
    replace: Replace,
) -> Result<Packed> {
    let output = match output {
        Some(o) => o,
        None => bundle::default_file_name()?,
    };
    // A destination nobody writes to fails before anything is packed.
    let sink = sinks.find(&output)?;
    // The tar is built where the sink can place it cheapest: for a file, beside it,
    // so a large bundle is not held in the scratch space as well.
    let scratch = match sink.staging(&output) {
        Some(dir) => TempDir::new_in(&dir, "pack-out"),
        None => TempDir::new("pack-out"),
    }?;
    let tar = scratch.path().join("bundle.tar");
    let manifest = bundle::pack::pack_to_file(config, &tar, tools)?;
    let written = sink.put(&output, &tar, manifest.arch()?, replace)?;
    let summary = format!(
        "packed {}, {} and {} for {} into {} (id {})",
        plural(manifest.binaries.len(), "binary", "binaries"),
        plural(manifest.data.len(), "data entry", "data entries"),
        plural(manifest.libs.len(), "library", "libraries"),
        manifest.arch,
        written,
        manifest.id()
    );
    let offered = offer(config, config_path, &output);
    Ok(Packed { summary, offered })
}

/// Lists `reference`, which was just written, in the config's `bundles`, so it can
/// be chosen later: unless it is already on offer, or there is no config file to
/// put it in (one is not made for this). The bundle is written either way, so a
/// config that cannot be changed is said and not an error. What is to be said of
/// it when it was put on offer is returned.
fn offer(config: &crate::config::Config, config_path: &Path, reference: &str) -> Option<String> {
    if !config_path.is_file() {
        return None;
    }
    let reference = crate::config::absolute_reference(reference);
    if on_offer(config, &reference) {
        return None;
    }
    match edit::add_bundle(config_path, &reference) {
        Ok(true) => Some(format!(
            "{reference} is on offer now (`bundles` in {})",
            config_path.display()
        )),
        Ok(false) => None,
        Err(e) => {
            warning!("could not put {reference} on offer in the config: {e:#}");
            None
        }
    }
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
            let manifest = Bundle::verified(&tar)?.manifest().clone();
            line!(
                "ok  {}, {}, {}, id {}",
                plural(manifest.binaries.len(), "binary", "binaries"),
                plural(manifest.data.len(), "data entry", "data entries"),
                plural(manifest.libs.len(), "library", "libraries"),
                manifest.id()
            );
            Ok(())
        }
        BundleCommand::Use { reference, live } => use_bundle(ctx, reference, live),
        BundleCommand::Add { reference } => add_bundle(ctx, &reference),
        BundleCommand::Rm { reference } => remove_bundle(ctx, &reference),
        BundleCommand::Ls => list_bundles(ctx),
    }
}

/// Asks which of `candidates` to use; `None` when the person declines.
pub fn pick(
    choose: &dyn Choose,
    question: &str,
    candidates: &[Offer],
    default: &Offer,
) -> Result<Option<Offer>> {
    let rows = candidates
        .iter()
        .map(|c| {
            let is_default = c == default;
            vec![
                Cell::new(c.label(), Tone::Strong),
                Cell::new(if is_default { "default" } else { "" }, Tone::Muted),
            ]
        })
        .collect();
    let picked = choose.choose(question, &Table::new(vec!["BUNDLE", ""], rows, 0))?;
    Ok(picked.map(|i| candidates[i].clone()))
}

/// Whether `reference` is the default or listed in `bundles`.
fn on_offer(config: &crate::config::Config, reference: &str) -> bool {
    choice::options(&choice::default(config), &config.bundles)
        .iter()
        .any(|c| c.reference() == Some(reference))
}

/// `ldg bundle use`: the bundle is `reference`, or the packed config, or one
/// picked from what is on offer.
fn use_bundle(ctx: &Ctx, reference: Option<String>, live: bool) -> Result<()> {
    let config = ctx.config()?;
    let can_pack = ctx.tools.packer.is_some();
    let new = match (reference, live) {
        (Some(reference), _) => Offer::Reference(crate::config::absolute_reference(&reference)),
        (None, true) => {
            anyhow::ensure!(
                can_pack,
                "this machine cannot pack the config (that needs Linux); give a bundle"
            );
            Offer::Live
        }
        (None, false) => {
            anyhow::ensure!(
                ctx.choose.can_ask(),
                "no terminal to choose on: give a bundle (`ldg bundle use FILE|oci://REF`) or --live"
            );
            let default = choice::default(&config);
            let offered = choice::selectable(&default, &config.bundles, can_pack);
            match pick(ctx.choose.as_ref(), "Use which bundle?", &offered, &default)? {
                Some(chosen) => chosen,
                None => return Ok(()),
            }
        }
    };
    adopt(ctx, &config, new)
}

/// Makes `new` the default. A bundle is fetched and checked before the config is
/// touched; the old default stays on offer.
fn adopt(ctx: &Ctx, config: &crate::config::Config, new: Offer) -> Result<()> {
    let old = choice::default(config);
    if new == old {
        note!("{} is the default already", new.label());
        return Ok(());
    }
    if let Offer::Reference(reference) = &new {
        let prepared =
            bundle::source::from_store(&bundle::cache::root()?, &ctx.bundles, reference)?;
        info!("bundle {} is ready", prepared.bundle.manifest().id());
    }
    // As written in the file, so what is kept keeps its form (`~/x.tar`).
    let path = &ctx.config_path;
    let written_old = edit::setting(path, Setting::Bundle)?
        .map(|b| b.trim_matches('"').to_string())
        .map_or(Offer::Live, Offer::Reference);
    let others = edit::bundles(path)?;
    edit::set_setting(path, Setting::Bundle, new.reference())?;
    let kept = choice::after_switch(&others, &written_old, &new);
    if kept != others {
        edit::set_bundles(path, &kept)?;
    }
    info!("using {}", new.label());
    if old.reference().is_some() {
        note!("{} stays on offer (`ldg bundle ls`)", old.label());
    }
    note!("a container that is injected already keeps its bundle until `ldg up --reinject`");
    Ok(())
}

/// `ldg bundle add`: puts a bundle on offer, after checking that it is there.
fn add_bundle(ctx: &Ctx, reference: &str) -> Result<()> {
    let config = ctx.config()?;
    let reference = crate::config::absolute_reference(reference);
    ctx.bundles
        .find(&reference)?
        .identify(&reference, Arch::host().ok())?;
    if on_offer(&config, &reference) {
        note!("{reference} is on offer already");
        return Ok(());
    }
    edit::add_bundle(&ctx.config_path, &reference)?;
    info!("{reference} is on offer");
    Ok(())
}

/// `ldg bundle rm`: takes a bundle off the list. The default is not on the list.
fn remove_bundle(ctx: &Ctx, reference: &str) -> Result<()> {
    let config = ctx.config()?;
    let absolute = crate::config::absolute_reference(reference);
    let path = &ctx.config_path;
    if edit::remove_bundle(path, reference)? || edit::remove_bundle(path, &absolute)? {
        info!("{reference} is not on offer any more");
        return Ok(());
    }
    if config.bundle.as_deref() == Some(&absolute) || config.bundle.as_deref() == Some(reference) {
        anyhow::bail!(
            "{reference} is the default: make another one the default first (`ldg bundle use`)"
        );
    }
    anyhow::bail!("{reference} is not on offer (`ldg bundle ls`)")
}

/// `ldg bundle ls`: what is on offer, the default first.
fn list_bundles(ctx: &Ctx) -> Result<()> {
    let config = ctx.config()?;
    let offered = choice::options(&choice::default(&config), &config.bundles);
    for (i, candidate) in offered.iter().enumerate() {
        let mark = if i == 0 { "*" } else { " " };
        line!("{mark} {}", candidate.label());
    }
    Ok(())
}

/// Prints what is in the bundle `reference` names, whose tar is at `file`;
/// `pinned` is the reference made exact, shown when it says more.
fn inspect(reference: &str, pinned: &str, file: &Path) -> Result<()> {
    let manifest = Bundle::open(file)?.manifest().clone();
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
    for b in &manifest.binaries {
        let kind = if b.dynamic { "relinked" } else { "as is" };
        let wrapper = match &b.dest {
            Some(dest) => format!(" + wrapper → {dest}"),
            None => String::new(),
        };
        line!("binary    {:<12} {kind}{wrapper}", b.name);
    }
    for d in &manifest.data {
        let kind = if d.is_dir { "dir" } else { "file" };
        let of = match &d.binary {
            Some(binary) => format!(" · for {binary}"),
            None => String::new(),
        };
        line!("data      {} ({kind}){of}", d.dest);
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
                "BINS",
                "SIZE"
            );
            for e in &entries {
                let (id, arch, tools) = match &e.manifest {
                    Some(m) => (m.id(), m.arch.to_string(), m.binaries.len().to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_at(text: &str) -> (TempDir, PathBuf, Config) {
        let dir = TempDir::new("bundle-offer").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let config = Config::load(&path).unwrap();
        (dir, path, config)
    }

    #[test]
    fn what_pack_writes_is_put_on_offer_in_the_config() {
        let (_dir, path, config) = config_at("target = \"docker\"\n");
        offer(&config, &path, "oci://ghcr.io/me/lodge:1");
        assert_eq!(edit::bundles(&path).unwrap(), ["oci://ghcr.io/me/lodge:1"]);
        // The default is not listed as well.
        let (_dir, path, config) = config_at("bundle = \"oci://r/x:1\"\n");
        offer(&config, &path, "oci://r/x:1");
        assert!(edit::bundles(&path).unwrap().is_empty());
    }

    #[test]
    fn a_bundle_on_offer_already_is_not_listed_again() {
        let (_dir, path, config) = config_at("bundles = [\"oci://r/x:1\"]\n");
        offer(&config, &path, "oci://r/x:1");
        assert_eq!(edit::bundles(&path).unwrap(), ["oci://r/x:1"]);
    }

    #[test]
    fn a_file_is_listed_by_its_absolute_path() {
        let (_dir, path, config) = config_at("");
        offer(&config, &path, "lodge-x86_64.tar");
        let listed = edit::bundles(&path).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(Path::new(&listed[0]).is_absolute(), "{listed:?}");
        assert!(listed[0].ends_with("lodge-x86_64.tar"));
    }

    #[test]
    fn there_is_no_config_file_made_just_to_list_a_bundle() {
        let dir = TempDir::new("bundle-offer-none").unwrap();
        let path = dir.path().join("config.toml");
        offer(&Config::default(), &path, "oci://r/x:1");
        assert!(!path.exists());
    }

    /// Picks the row it is told to, after checking which one it was shown as the
    /// default.
    struct Row(Option<usize>);
    impl Choose for Row {
        fn choose(&self, _: &str, options: &Table) -> Result<Option<usize>> {
            let rows = options.natural();
            assert!(
                rows[0].contains("a.tar") && rows[0].contains("default"),
                "{rows:?}"
            );
            assert!(!rows[1].contains("default"), "{rows:?}");
            Ok(self.0)
        }
    }

    #[test]
    fn the_picker_gives_back_the_offer_that_was_chosen() {
        let offered = [Offer::Reference("a.tar".into()), Offer::Live];
        let default = offered[0].clone();
        assert_eq!(
            pick(&Row(Some(1)), "?", &offered, &default).unwrap(),
            Some(Offer::Live)
        );
        assert_eq!(pick(&Row(None), "?", &offered, &default).unwrap(), None);
    }
}
