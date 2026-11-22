//! Getting a bundle ready to inject from.
//!
//! Injection reads only from a [`Bundle`]: a verified tar, read in place. This
//! module produces one, from either source, and keeps it in the [`cache`] so
//! work is not repeated:
//!
//! - a bundle **reference** (`--bundle`, or `bundle = …` in the config: a file, a
//!   registry, whatever a [`store`](super::store) handles) is identified, and
//!   fetched and verified once, keyed by what it identified as (the hash of a
//!   file, the digest of an image);
//! - otherwise the config is **packed live**, keyed by a fingerprint of everything
//!   it depends on (the config entries, each source file's size and mtime, the
//!   libraries `ldd` finds). Working out that fingerprint runs `ldd`; if it is
//!   unchanged the cached bundle is reused and nothing is copied or relinked
//!   (`patchelf` does not run).
//!
//! So the live path and the stored-bundle path end in the same place, and there is
//! a single injector.

use anyhow::{Context as _, Result};
use std::path::Path;

use super::manifest::Manifest;
use super::reader::Bundle;
use super::store::{Stores, Unreachable};
use super::{cache, deps, elf, pack};
use crate::arch::Arch;
use crate::config::Config;
use crate::fingerprint::Fingerprint;
use crate::output::{info, warning};
use crate::platform;
use crate::snapshot::mtime;
use crate::tools::Toolchain;
use crate::tree::{self, Denied, Links};

/// A bundle ready to inject from.
pub struct Prepared {
    pub bundle: Bundle,
    pub origin: Origin,
    /// Where it came from, as exactly as the place can say (a registry's digest),
    /// for `ldg status`; `None` for one packed from the config.
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Reused from the cache; nothing was packed or fetched.
    Cached,
    /// Packed from the config just now.
    Packed,
    /// Fetched from a store (a file, a registry) and verified just now.
    Fetched,
}

/// Readies the bundle to inject from: `reference` if given, else the config's
/// `bundle`, else a live pack of the config.
pub fn prepare(
    config: &Config,
    reference: Option<&str>,
    stores: &Stores,
    tools: &Toolchain,
) -> Result<Prepared> {
    prepare_in(&cache::root()?, config, reference, stores, tools)
}

pub(crate) fn prepare_in(
    root: &Path,
    config: &Config,
    reference: Option<&str>,
    stores: &Stores,
    tools: &Toolchain,
) -> Result<Prepared> {
    let explicit = reference.or(config.bundle.as_deref());
    let prepared = match explicit {
        Some(reference) => {
            if !config.binaries.is_empty() || !config.data.is_empty() {
                warning!(
                    "a bundle is in use, so the [[binary]] and [[data]] entries in the config are ignored"
                );
            }
            from_store(root, stores, reference)?
        }
        None => live(root, config, tools)?,
    };
    cache::prune(root, prepared.bundle.file());

    let how = match prepared.origin {
        Origin::Cached => "cached",
        Origin::Packed => "packed",
        Origin::Fetched => "fetched",
    };
    info!("bundle {} ({how})", prepared.bundle.manifest().id());
    Ok(prepared)
}

/// The bundle `reference` names, from whichever store owns it: identified
/// first (cheap), and only fetched when the cache has not got that content.
pub(crate) fn from_store(root: &Path, stores: &Stores, reference: &str) -> Result<Prepared> {
    let store = stores.find(reference)?;
    // A CPU lodge does not know cannot pick an image, but can read a file.
    let arch = Arch::host().ok();
    let identity = match store.identify(reference, arch) {
        Ok(identity) => identity,
        // A place that cannot be reached at all is answered from the cache, with
        // what the reference named the last time it could be asked. Nothing else
        // is: a place that refuses or has no such bundle is the answer.
        Err(e) if e.downcast_ref::<Unreachable>().is_some() => {
            let Some(key) = cache::recall(root, reference, arch) else {
                return Err(e.context(format!(
                    "{reference} is not in the cache either, so there is nothing to use offline"
                )));
            };
            warning!(
                "cannot reach {reference} ({}); using the bundle it named last time ({key})",
                short(&e)
            );
            let mut prepared = cached(root, &key)?;
            prepared.source = Some(format!("{reference} as it was last time ({key})"));
            return Ok(prepared);
        }
        Err(e) => return Err(e),
    };
    if let Some(bundle) = cache::load(root, &identity.key) {
        cache::remember(root, reference, arch, &identity.key);
        return Ok(Prepared {
            bundle,
            origin: Origin::Cached,
            source: Some(identity.pinned),
        });
    }
    let staging = cache::staging(root, "fetch")?;
    let fetched = store.fetch(&identity, staging.path())?;
    // What is verified is the copy that is kept, so nothing can change between.
    let staged = cache::stage_tar(&staging, &fetched)?;
    Bundle::verified(&staged)?;
    let bundle = cache::install(root, staging, &identity.key)?;
    cache::remember(root, reference, arch, &identity.key);
    Ok(Prepared {
        bundle,
        origin: Origin::Fetched,
        source: Some(identity.pinned),
    })
}

/// What a reference names, found without fetching or extracting anything: for a
/// check that must have no side effects.
pub struct Lookup {
    /// The reference made exact (a registry's tag as the digest it names).
    pub pinned: String,
    /// The bundle, when the cache holds that content.
    pub cached: Option<Manifest>,
    /// The place could not be reached, so this is what it named last time.
    pub offline: bool,
}

/// Looks `reference` up in its store (and in the cache), and changes nothing.
pub(crate) fn look_up(root: &Path, stores: &Stores, reference: &str) -> Result<Lookup> {
    let store = stores.find(reference)?;
    let arch = Arch::host().ok();
    match store.identify(reference, arch) {
        Ok(identity) => Ok(Lookup {
            cached: cache::load(root, &identity.key).map(|b| b.manifest().clone()),
            pinned: identity.pinned,
            offline: false,
        }),
        Err(e) if e.downcast_ref::<Unreachable>().is_some() => {
            let remembered =
                cache::recall(root, reference, arch).and_then(|key| cache::load(root, &key));
            match remembered {
                Some(bundle) => Ok(Lookup {
                    pinned: reference.into(),
                    cached: Some(bundle.manifest().clone()),
                    offline: true,
                }),
                None => Err(e.context(format!(
                    "{reference} is not in the cache either, so there is nothing to use offline"
                ))),
            }
        }
        Err(e) => Err(e),
    }
}

/// The bundle in the cache under `key`.
fn cached(root: &Path, key: &str) -> Result<Prepared> {
    let bundle =
        cache::load(root, key).with_context(|| format!("the cached bundle {key} is gone"))?;
    Ok(Prepared {
        bundle,
        origin: Origin::Cached,
        source: None,
    })
}

/// The first line of what went wrong, for a warning.
fn short(e: &anyhow::Error) -> String {
    e.root_cause().to_string()
}

fn live(root: &Path, config: &Config, tools: &Toolchain) -> Result<Prepared> {
    let packer = tools.packer()?;
    // Shared by the fingerprint and the pack, so the libraries of a binary are
    // looked up once.
    let mut deps = deps::Resolver::new(packer.deps.as_ref());
    let key = format!("p-{}", inputs_fingerprint(config, &mut deps)?);
    if let Some(bundle) = cache::load(root, &key) {
        return Ok(Prepared {
            bundle,
            origin: Origin::Cached,
            source: None,
        });
    }
    let staging = cache::staging(root, "pack")?;
    pack::build_tar(
        config,
        &cache::staged(&staging),
        &mut deps,
        packer.elf.as_ref(),
    )?;
    Ok(Prepared {
        bundle: cache::install(root, staging, &key)?,
        origin: Origin::Packed,
        source: None,
    })
}

/// A fingerprint of everything a live pack depends on. Packing is skipped when it
/// is unchanged.
///
/// [`pack::build`] reads the `[[binary]]` and `[[data]]` entries and `[git]`, and the files they
/// name. The entries and settings go in whole, serialized, so a
/// setting added to them later counts without anyone remembering to list it
/// here; the files go in by size, mtime and mode.
fn inputs_fingerprint(config: &Config, deps: &mut deps::Resolver<'_>) -> Result<String> {
    let host = Arch::host()?;
    let mut h = Fingerprint::new();
    h.field(env!("CARGO_PKG_VERSION").as_bytes())
        .field(host.to_string().as_bytes());

    let mut parts = Vec::new();
    for item in &config.binaries {
        parts.push(serde_json::to_string(item)?);
        // Before ldd, which cannot inspect a binary for another architecture.
        elf::check_arch(&item.name, &item.path, host)?;
        parts.push(stat_line(&item.path)?);
        if item.copy_deps {
            for lib in &deps.resolve(&item.path.to_string_lossy())?.libs {
                parts.push(stat_line(lib)?);
            }
        }
    }
    for item in &config.data {
        parts.push(serde_json::to_string(item)?);
        stat_tree(&item.path, &mut parts)?;
    }
    parts.push(serde_json::to_string(&config.git)?);

    for part in &parts {
        h.field(part.as_bytes());
    }
    Ok(h.hex())
}

/// `path size mtime mode`, following symlinks to the file that is really used.
fn stat_line(path: &Path) -> Result<String> {
    use anyhow::Context as _;
    let real =
        std::fs::canonicalize(path).with_context(|| format!("reading {}", path.display()))?;
    let meta = std::fs::metadata(&real)?;
    let (secs, nanos) = mtime(&meta);
    Ok(format!(
        "{} {} {secs}.{nanos} {:o}",
        real.display(),
        meta.len(),
        platform::mode(&meta)
    ))
}

/// Adds one line per entry of the tree at `root` (by name order): enough to notice
/// that anything under it changed without reading file contents.
fn stat_tree(root: &Path, out: &mut Vec<String>) -> Result<()> {
    use anyhow::Context as _;
    let real = std::fs::canonicalize(root)
        .with_context(|| format!("config path {} does not exist", root.display()))?;
    tree::walk(&real, Links::Keep, Denied::Fail, &mut |e| {
        let (relative, meta) = (e.relative, e.meta);
        out.push(if meta.is_symlink() {
            format!("L {relative} -> {}", std::fs::read_link(e.path)?.display())
        } else if meta.is_dir() {
            format!("D {relative} {:o}", platform::mode(meta))
        } else {
            let (secs, nanos) = mtime(meta);
            format!(
                "F {relative} {} {secs}.{nanos} {:o}",
                meta.len(),
                platform::mode(meta)
            )
        });
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::store::fake::FakeStore;
    use crate::bundle::store::{FileStore, OciStore};
    use crate::bundle::testing::{manifest, prepared};
    use crate::config::BinaryEntry;
    use crate::snapshot::Snapshot;
    use crate::tmpfile::TempDir;
    use crate::tools::{self, FakeRegistry, Ldd};
    use std::sync::Arc;

    fn files() -> Stores {
        Stores::new(vec![Box::new(FileStore)])
    }

    /// A packed bundle, as the bytes of its tar.
    fn bundle_tar(dir: &Path, body: &str) -> Vec<u8> {
        let out = dir.join("packed.tar");
        let cfg = config(vec![script_tool(dir, "t", body)]);
        pack::pack_to_file(&cfg, &out, &tools::external()).unwrap();
        std::fs::read(out).unwrap()
    }

    fn script_tool(dir: &Path, name: &str, body: &str) -> BinaryEntry {
        let bin = dir.join(format!("{name}-src"));
        std::fs::write(&bin, body).unwrap();
        BinaryEntry {
            name: name.into(),
            path: bin,
            target: None,
            copy_deps: false,
        }
    }

    fn config(binaries: Vec<BinaryEntry>) -> Config {
        Config {
            binaries,
            ..Config::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_second_prepare_of_unchanged_inputs_reuses_the_cache() {
        let work = TempDir::new("src").unwrap();
        let cache = TempDir::new("cache").unwrap();
        let cfg = config(vec![script_tool(work.path(), "t", "#!/bin/sh\necho 1\n")]);

        let first = prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap();
        assert_eq!(first.origin, Origin::Packed);
        let second = prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap();
        assert_eq!(second.origin, Origin::Cached);
        assert_eq!(first.bundle.file(), second.bundle.file());
        assert_eq!(first.bundle.manifest().id(), second.bundle.manifest().id());
    }

    #[cfg(unix)]
    #[test]
    fn changing_a_source_file_packs_a_new_bundle() {
        let work = TempDir::new("src2").unwrap();
        let cache = TempDir::new("cache2").unwrap();
        let tool = script_tool(work.path(), "t", "#!/bin/sh\necho 1\n");
        let source = tool.path.clone();
        let cfg = config(vec![tool]);

        let first = prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap();
        // A different size, so the change is noticed whatever the mtime resolution.
        std::fs::write(&source, "#!/bin/sh\necho changed, and longer\n").unwrap();
        let second = prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap();
        assert_eq!(second.origin, Origin::Packed);
        assert_ne!(first.bundle.file(), second.bundle.file());
        assert_ne!(first.bundle.manifest().id(), second.bundle.manifest().id());
    }

    #[cfg(unix)]
    #[test]
    fn a_change_in_a_data_tree_is_noticed() {
        let work = TempDir::new("src3").unwrap();
        let cache = TempDir::new("cache3").unwrap();
        let tool = script_tool(work.path(), "t", "#!/bin/sh\n");
        let cfg_dir = work.path().join("cfgdir");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("rc"), "a").unwrap();
        let mut cfg = config(vec![tool]);
        cfg.data.push(crate::config::DataEntry {
            path: cfg_dir.clone(),
            target: None,
            binary: Some("t".into()),
        });

        let first = prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap();
        std::fs::write(cfg_dir.join("added"), "new file").unwrap();
        let second = prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap();
        assert_eq!(second.origin, Origin::Packed);
        assert_ne!(first.bundle.file(), second.bundle.file());
    }

    #[test]
    fn every_setting_of_an_entry_is_part_of_the_fingerprint() {
        let work = TempDir::new("src7").unwrap();
        let base = || script_tool(work.path(), "t", "#!/bin/sh\n");
        let key = |item: BinaryEntry| {
            inputs_fingerprint(&config(vec![item]), &mut deps::Resolver::new(&Ldd)).unwrap()
        };
        let plain = key(base());
        // (`copy_deps` too, but turning it on asks ldd about the binary.)
        let mut item = base();
        item.target = Some("/opt/t".into());
        assert_ne!(key(item), plain);
    }

    #[test]
    fn git_settings_are_part_of_the_fingerprint() {
        let a = config(vec![]);
        let mut b = config(vec![]);
        b.git.safe_directories = vec!["/work".into()];
        assert_ne!(
            inputs_fingerprint(&a, &mut deps::Resolver::new(&Ldd)).unwrap(),
            inputs_fingerprint(&b, &mut deps::Resolver::new(&Ldd)).unwrap()
        );
        assert_eq!(
            inputs_fingerprint(&a, &mut deps::Resolver::new(&Ldd)).unwrap(),
            inputs_fingerprint(&config(vec![]), &mut deps::Resolver::new(&Ldd)).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_file_is_fetched_once_and_then_reused() {
        let work = TempDir::new("src4").unwrap();
        let out = TempDir::new("out4").unwrap();
        let cache = TempDir::new("cache4").unwrap();
        let cfg = config(vec![script_tool(work.path(), "t", "#!/bin/sh\n")]);
        let file = out.path().join("b.tar");
        pack::pack_to_file(&cfg, &file, &tools::external()).unwrap();

        // The config used to *inject* has no tools: the bundle supplies them.
        let empty = config(vec![]);
        let first = prepare_in(
            cache.path(),
            &empty,
            Some(file.to_str().unwrap()),
            &files(),
            &tools::external(),
        )
        .unwrap();
        assert_eq!(first.origin, Origin::Fetched);
        assert_eq!(first.bundle.manifest().binaries.len(), 1);
        assert!(first.bundle.bin("t").root().is_ok());
        let second = prepare_in(
            cache.path(),
            &empty,
            Some(file.to_str().unwrap()),
            &files(),
            &tools::external(),
        )
        .unwrap();
        assert_eq!(second.origin, Origin::Cached);
    }

    #[cfg(unix)]
    #[test]
    fn a_damaged_bundle_file_is_refused_and_leaves_nothing_cached() {
        let work = TempDir::new("src5").unwrap();
        let out = TempDir::new("out5").unwrap();
        let cache = TempDir::new("cache5").unwrap();
        let cfg = config(vec![script_tool(
            work.path(),
            "t",
            "#!/bin/sh\necho payload\n",
        )]);
        let file = out.path().join("b.tar");
        pack::pack_to_file(&cfg, &file, &tools::external()).unwrap();
        let mut bytes = std::fs::read(&file).unwrap();
        // Damage the binary's content: the manifest is read first, and must be whole.
        let pos = bytes.windows(7).position(|w| w == b"payload").unwrap();
        bytes[pos] ^= 0xFF;
        std::fs::write(&file, bytes).unwrap();

        let err = prepare_in(
            cache.path(),
            &config(vec![]),
            Some(file.to_str().unwrap()),
            &files(),
            &tools::external(),
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("damaged"), "{err:#}");
        let leftovers: Vec<_> = std::fs::read_dir(cache.path()).unwrap().collect();
        assert!(
            leftovers.is_empty(),
            "a failed fetch left {} entries",
            leftovers.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_can_be_listed_and_cleared() {
        let tmp = TempDir::new("cache-list").unwrap();
        let root = tmp.path().join("bundles");
        let script = script_tool(tmp.path(), "hello", "echo hi");
        prepare_in(
            &root,
            &config(vec![script]),
            None,
            &files(),
            &tools::external(),
        )
        .unwrap();
        let entries = cache::entries(&root);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].manifest.is_some());
        assert!(entries[0].size > 0);
        let (n, size) = cache::clear(&root);
        assert_eq!((n, size > 0), (1, true));
        assert!(cache::entries(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_most_recently_used_bundles_are_kept() {
        let work = TempDir::new("src6").unwrap();
        let cache = TempDir::new("cache6").unwrap();
        let mut last = None;
        for i in 0..(cache::KEEP + 2) {
            let cfg = config(vec![script_tool(
                work.path(),
                "t",
                &format!("#!/bin/sh\necho {i}{}\n", "x".repeat(i)),
            )]);
            last =
                Some(prepare_in(cache.path(), &cfg, None, &files(), &tools::external()).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let kept = std::fs::read_dir(cache.path()).unwrap().count();
        assert_eq!(kept, cache::KEEP);
        assert!(
            last.unwrap().bundle.file().exists(),
            "the bundle in use must survive pruning"
        );
    }

    #[cfg(unix)]
    #[test]
    fn looking_a_bundle_up_fetches_nothing_and_says_whether_it_is_cached() {
        let work = TempDir::new("lookup").unwrap();
        let cache = TempDir::new("cache-lookup").unwrap();
        let fake = FakeStore::new().with("a", "x-1", bundle_tar(work.path(), "#!/bin/sh\n"));
        let fetched = fake.fetched();
        let stores = Stores::new(vec![Box::new(fake)]);

        let before = look_up(cache.path(), &stores, "fake://a").unwrap();
        assert!(before.cached.is_none() && !before.offline);
        assert_eq!(before.pinned, "fake://a");
        assert!(fetched.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);

        prepare_a(&cache, &stores).unwrap();
        let after = look_up(cache.path(), &stores, "fake://a").unwrap();
        assert!(after.cached.is_some());
        assert_eq!(fetched.lock().unwrap().len(), 1, "only `prepare` fetched");
    }

    #[cfg(unix)]
    #[test]
    fn looking_up_a_place_that_cannot_be_reached_uses_what_was_remembered() {
        let work = TempDir::new("lookup2").unwrap();
        let cache = TempDir::new("cache-lookup2").unwrap();
        let online = Stores::new(vec![Box::new(FakeStore::new().with(
            "a",
            "x-1",
            bundle_tar(work.path(), "#!/bin/sh\n"),
        ))]);
        let offline = Stores::new(vec![Box::new(FakeStore::new().unreachable())]);
        assert!(look_up(cache.path(), &offline, "fake://a").is_err());
        prepare_a(&cache, &online).unwrap();
        let found = look_up(cache.path(), &offline, "fake://a").unwrap();
        assert!(found.offline && found.cached.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_from_a_store_is_fetched_once_then_found_in_the_cache() {
        let work = TempDir::new("store1").unwrap();
        let cache = TempDir::new("cache-s1").unwrap();
        let tar = bundle_tar(work.path(), "#!/bin/sh\n");
        let fake = FakeStore::new().with("a", "x-1", tar);
        let fetched = fake.fetched();
        let stores = Stores::new(vec![Box::new(fake)]);
        let empty = config(vec![]);

        let first = prepare_in(
            cache.path(),
            &empty,
            Some("fake://a"),
            &stores,
            &tools::external(),
        )
        .unwrap();
        assert_eq!(first.origin, Origin::Fetched);
        assert!(first.bundle.bin("t").root().is_ok());
        assert!(
            first.bundle.file().ends_with("x-1.tar"),
            "{}",
            first.bundle.file().display()
        );
        let second = prepare_in(
            cache.path(),
            &empty,
            Some("fake://a"),
            &stores,
            &tools::external(),
        )
        .unwrap();
        assert_eq!(second.origin, Origin::Cached);
        assert_eq!(fetched.lock().unwrap().len(), 1);
        // Nothing is left from fetching: the bundle, and the note of what the
        // reference named.
        let mut names: Vec<_> = std::fs::read_dir(cache.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["tags.json", "x-1.tar"]);
    }

    /// Prepares `fake://a` from `stores` into `cache`.
    fn prepare_a(cache: &TempDir, stores: &Stores) -> Result<Prepared> {
        prepare_in(
            cache.path(),
            &config(vec![]),
            Some("fake://a"),
            stores,
            &tools::external(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn a_place_that_cannot_be_reached_is_answered_from_the_cache_with_what_it_named_last() {
        let work = TempDir::new("offline1").unwrap();
        let cache = TempDir::new("cache-off1").unwrap();
        let online = Stores::new(vec![Box::new(FakeStore::new().with(
            "a",
            "x-1",
            bundle_tar(work.path(), "#!/bin/sh\n"),
        ))]);
        assert_eq!(prepare_a(&cache, &online).unwrap().origin, Origin::Fetched);

        let offline = FakeStore::new().unreachable();
        let fetched = offline.fetched();
        let offline = Stores::new(vec![Box::new(offline)]);
        let again = prepare_a(&cache, &offline).unwrap();
        assert_eq!(again.origin, Origin::Cached);
        assert!(
            again.bundle.file().ends_with("x-1.tar"),
            "{}",
            again.bundle.file().display()
        );
        assert!(again.bundle.bin("t").root().is_ok());
        assert!(
            fetched.lock().unwrap().is_empty(),
            "nothing is fetched offline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn offline_with_nothing_remembered_or_nothing_cached_is_an_error_that_says_so() {
        let work = TempDir::new("offline2").unwrap();
        let offline = Stores::new(vec![Box::new(FakeStore::new().unreachable())]);
        let err = prepare_a(&TempDir::new("cache-off2").unwrap(), &offline)
            .err()
            .unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("not in the cache either") && text.contains("no route to host"),
            "{text}"
        );

        // Remembered, but pruned since: also nothing to use.
        let cache = TempDir::new("cache-off3").unwrap();
        let online = Stores::new(vec![Box::new(FakeStore::new().with(
            "a",
            "x-1",
            bundle_tar(work.path(), "#!/bin/sh\n"),
        ))]);
        prepare_a(&cache, &online).unwrap();
        std::fs::remove_file(cache.path().join("x-1.tar")).unwrap();
        assert!(prepare_a(&cache, &offline).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_place_that_answers_no_is_not_overruled_by_the_cache() {
        let work = TempDir::new("offline3").unwrap();
        let cache = TempDir::new("cache-off4").unwrap();
        let online = Stores::new(vec![Box::new(FakeStore::new().with(
            "a",
            "x-1",
            bundle_tar(work.path(), "#!/bin/sh\n"),
        ))]);
        prepare_a(&cache, &online).unwrap();
        // Reachable, and it has no such bundle (as a 404 or a refusal says).
        let refusing = Stores::new(vec![Box::new(FakeStore::new())]);
        let err = prepare_a(&cache, &refusing).err().unwrap();
        assert!(err.to_string().contains("no such fake bundle"), "{err}");
    }

    #[test]
    fn the_same_reference_for_another_architecture_is_remembered_apart() {
        let root = TempDir::new("tags").unwrap();
        // `recall` needs a real bundle in the entry.
        let (_dir, bundle) = prepared(&manifest("x86_64"), &[]);
        for key in ["o-a", "o-b"] {
            std::fs::copy(bundle.bundle.file(), root.path().join(format!("{key}.tar"))).unwrap();
        }
        cache::remember(root.path(), "oci://x/y:1", Some(Arch::X86_64), "o-a");
        cache::remember(root.path(), "oci://x/y:1", Some(Arch::Aarch64), "o-b");
        assert_eq!(
            cache::recall(root.path(), "oci://x/y:1", Some(Arch::Aarch64)).as_deref(),
            Some("o-b")
        );
        assert_eq!(
            cache::recall(root.path(), "oci://x/y:1", Some(Arch::X86_64)).as_deref(),
            Some("o-a")
        );
        assert_eq!(cache::recall(root.path(), "oci://x/y:2", None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_in_a_registry_is_pulled_once_per_digest() {
        let work = TempDir::new("store2").unwrap();
        let cache = TempDir::new("cache-s2").unwrap();
        let digest = format!("sha256:{}", "ab".repeat(32));
        let registry = Arc::new(FakeRegistry::new().with(
            "ghcr.io/me/lodge:1",
            &digest,
            &bundle_tar(work.path(), "#!/bin/sh\n"),
        ));
        let stores = Stores::new(vec![
            Box::new(FileStore),
            Box::new(OciStore::new(registry.clone())),
        ]);
        let empty = config(vec![]);
        let reference = "oci://ghcr.io/me/lodge:1";

        let first = prepare_in(
            cache.path(),
            &empty,
            Some(reference),
            &stores,
            &tools::external(),
        )
        .unwrap();
        assert_eq!(first.origin, Origin::Fetched);
        assert!(first.bundle.bin("t").root().is_ok());
        let second = prepare_in(
            cache.path(),
            &empty,
            Some(reference),
            &stores,
            &tools::external(),
        )
        .unwrap();
        assert_eq!(second.origin, Origin::Cached);
        assert_eq!(
            registry.pulled(),
            vec![format!("ghcr.io/me/lodge:1@{digest}")]
        );
    }

    #[test]
    fn the_config_names_a_bundle_too_and_an_unknown_scheme_is_refused() {
        let cache = TempDir::new("cache-s3").unwrap();
        let cfg = Config {
            bundle: Some("ftp://host/b.tar".into()),
            ..Config::default()
        };
        let err = prepare_in(cache.path(), &cfg, None, &files(), &tools::external())
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("a file path"), "{err:#}");
    }
    #[cfg(unix)]
    #[test]
    fn a_machine_that_cannot_pack_still_uses_a_bundle_but_cannot_build_one() {
        let work = TempDir::new("consumer").unwrap();
        let out = TempDir::new("consumer-out").unwrap();
        let cache = TempDir::new("consumer-cache").unwrap();
        let file = out.path().join("b.tar");
        let cfg = config(vec![script_tool(work.path(), "t", "#!/bin/sh\n")]);
        pack::pack_to_file(&cfg, &file, &tools::external()).unwrap();
        let tools = tools::consumer_only();

        // A bundle is used...
        let used = prepare_in(
            cache.path(),
            &config(vec![]),
            Some(file.to_str().unwrap()),
            &files(),
            &tools,
        )
        .unwrap();
        assert_eq!(used.origin, Origin::Fetched);
        // ...but there is nothing to build one from the config with, and it says so.
        let err = prepare_in(cache.path(), &cfg, None, &files(), &tools)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("cannot build a bundle"), "{err}");
        let out = out.path().join("again.tar");
        assert!(pack::pack_to_file(&cfg, &out, &tools).is_err());
        assert!(!out.exists());
    }
}
