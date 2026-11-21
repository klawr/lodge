//! Getting a bundle ready to inject from.
//!
//! Injection reads only from a bundle *directory*: a manifest plus `bin/`, `lib/`
//! and `config/`. This module produces one, from either source, and keeps it in
//! the [`cache`] so work is not repeated:
//!
//! - a bundle **file** (`--bundle`, or `bundle = …` in the config) is extracted
//!   once, keyed by the hash of the file;
//! - otherwise the config is **packed live**, keyed by a fingerprint of everything
//!   it depends on (the config entries, each source file's size and mtime, the
//!   libraries `ldd` finds). Working out that fingerprint runs `ldd`; if it is
//!   unchanged the cached bundle is reused and nothing is copied or relinked
//!   (`patchelf` does not run).
//!
//! So the live path and the bundle-file path end in the same place, and there is a
//! single injector.

use anyhow::Result;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::hash::hash_path;
use super::manifest::{BIN_DIR, CONFIG_DIR, LIB_DIR, Manifest};
use super::{archive, cache, deps, elf, pack};
use crate::bundle::arch::Arch;
use crate::config::Config;
use crate::fingerprint::Fingerprint;
use crate::output::{info, warning};
use crate::tree::{self, Denied, Links};

/// A bundle directory ready to inject from.
pub struct Prepared {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub origin: Origin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Reused from the cache; nothing was packed or extracted.
    Cached,
    /// Packed from the config just now.
    Packed,
    /// Extracted from a bundle file just now.
    Extracted,
}

impl Prepared {
    pub fn bin(&self, tool: &str) -> PathBuf {
        self.dir.join(BIN_DIR).join(tool)
    }

    pub fn lib(&self, name: &str) -> PathBuf {
        self.dir.join(LIB_DIR).join(name)
    }

    pub fn config(&self, tool: &str) -> PathBuf {
        self.dir.join(CONFIG_DIR).join(tool)
    }
}

/// Readies the bundle to inject from: `file` if given, else the config's
/// `bundle`, else a live pack of the config.
pub fn prepare(config: &Config, file: Option<&Path>) -> Result<Prepared> {
    prepare_in(&cache::root()?, config, file)
}

pub(crate) fn prepare_in(root: &Path, config: &Config, file: Option<&Path>) -> Result<Prepared> {
    let explicit = file
        .map(Path::to_path_buf)
        .or_else(|| config.bundle.clone());
    let prepared = match explicit {
        Some(file) => {
            if !config.inject.is_empty() {
                warning!("a bundle is in use, so the [[inject]] entries in the config are ignored");
            }
            from_file(root, &file)?
        }
        None => live(root, config)?,
    };
    cache::prune(root, &prepared.dir);

    let how = match prepared.origin {
        Origin::Cached => "cached",
        Origin::Packed => "packed",
        Origin::Extracted => "extracted",
    };
    info!("bundle {} ({how})", prepared.manifest.id());
    Ok(prepared)
}

fn from_file(root: &Path, file: &Path) -> Result<Prepared> {
    let dir = root.join(format!("f-{}", hash_path(file)?));
    if let Some(manifest) = cache::load(&dir) {
        return Ok(Prepared {
            dir,
            manifest,
            origin: Origin::Cached,
        });
    }
    let staging = cache::staging(root, "extract")?;
    let manifest = archive::extract(file, staging.path())?;
    Ok(Prepared {
        dir: cache::install(staging, &dir)?,
        manifest,
        origin: Origin::Extracted,
    })
}

fn live(root: &Path, config: &Config) -> Result<Prepared> {
    // Shared by the fingerprint and the pack, so `ldd` runs once per binary.
    let mut deps = deps::Resolver::default();
    let dir = root.join(format!("p-{}", inputs_fingerprint(config, &mut deps)?));
    if let Some(manifest) = cache::load(&dir) {
        return Ok(Prepared {
            dir,
            manifest,
            origin: Origin::Cached,
        });
    }
    let staging = cache::staging(root, "pack")?;
    let manifest = pack::build(config, staging.path(), &mut deps)?;
    Ok(Prepared {
        dir: cache::install(staging, &dir)?,
        manifest,
        origin: Origin::Packed,
    })
}

/// A fingerprint of everything a live pack depends on. Packing is skipped when it
/// is unchanged.
///
/// [`pack::build`] reads the `[[inject]]` entries and `[git]`, and the files they
/// name. The entries and settings go in whole, serialized, so a
/// setting added to them later counts without anyone remembering to list it
/// here; the files go in by size, mtime and mode.
fn inputs_fingerprint(config: &Config, deps: &mut deps::Resolver) -> Result<String> {
    let host = Arch::host()?;
    let mut h = Fingerprint::new();
    h.field(env!("CARGO_PKG_VERSION").as_bytes())
        .field(host.to_string().as_bytes());

    let mut parts = Vec::new();
    for item in &config.inject {
        parts.push(serde_json::to_string(item)?);
        if let Some(binary) = &item.binary {
            // Before ldd, which cannot inspect a binary for another architecture.
            elf::check_arch(&item.name, binary, host)?;
            parts.push(stat_line(binary)?);
            if item.copy_deps {
                for lib in &deps.resolve(&binary.to_string_lossy())?.libs {
                    parts.push(stat_line(lib)?);
                }
            }
        }
        if let Some(cfg) = &item.config {
            stat_tree(cfg, &mut parts)?;
        }
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
    Ok(format!(
        "{} {} {}.{} {:o}",
        real.display(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.mode()
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
            format!("D {relative} {:o}", meta.mode())
        } else {
            format!(
                "F {relative} {} {}.{} {:o}",
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec(),
                meta.mode()
            )
        });
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Injectable;
    use crate::tmpfile::TempDir;

    fn script_tool(dir: &Path, name: &str, body: &str) -> Injectable {
        let bin = dir.join(format!("{name}-src"));
        std::fs::write(&bin, body).unwrap();
        Injectable {
            name: name.into(),
            binary: Some(bin),
            config: None,
            target_binary: None,
            target_config: None,
            skip_if_exists: false,
            copy_deps: false,
        }
    }

    fn config(inject: Vec<Injectable>) -> Config {
        Config {
            inject,
            ..Config::default()
        }
    }

    #[test]
    fn the_second_prepare_of_unchanged_inputs_reuses_the_cache() {
        let work = TempDir::new("src").unwrap();
        let cache = TempDir::new("cache").unwrap();
        let cfg = config(vec![script_tool(work.path(), "t", "#!/bin/sh\necho 1\n")]);

        let first = prepare_in(cache.path(), &cfg, None).unwrap();
        assert_eq!(first.origin, Origin::Packed);
        let second = prepare_in(cache.path(), &cfg, None).unwrap();
        assert_eq!(second.origin, Origin::Cached);
        assert_eq!(first.dir, second.dir);
        assert_eq!(first.manifest.id(), second.manifest.id());
    }

    #[test]
    fn changing_a_source_file_packs_a_new_bundle() {
        let work = TempDir::new("src2").unwrap();
        let cache = TempDir::new("cache2").unwrap();
        let tool = script_tool(work.path(), "t", "#!/bin/sh\necho 1\n");
        let source = tool.binary.clone().unwrap();
        let cfg = config(vec![tool]);

        let first = prepare_in(cache.path(), &cfg, None).unwrap();
        // A different size, so the change is noticed whatever the mtime resolution.
        std::fs::write(&source, "#!/bin/sh\necho changed, and longer\n").unwrap();
        let second = prepare_in(cache.path(), &cfg, None).unwrap();
        assert_eq!(second.origin, Origin::Packed);
        assert_ne!(first.dir, second.dir);
        assert_ne!(first.manifest.id(), second.manifest.id());
    }

    #[test]
    fn a_change_in_the_config_tree_is_noticed() {
        let work = TempDir::new("src3").unwrap();
        let cache = TempDir::new("cache3").unwrap();
        let mut tool = script_tool(work.path(), "t", "#!/bin/sh\n");
        let cfg_dir = work.path().join("cfgdir");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("rc"), "a").unwrap();
        tool.config = Some(cfg_dir.clone());
        let cfg = config(vec![tool]);

        let first = prepare_in(cache.path(), &cfg, None).unwrap();
        std::fs::write(cfg_dir.join("added"), "new file").unwrap();
        let second = prepare_in(cache.path(), &cfg, None).unwrap();
        assert_eq!(second.origin, Origin::Packed);
        assert_ne!(first.dir, second.dir);
    }

    #[test]
    fn every_setting_of_an_entry_is_part_of_the_fingerprint() {
        let work = TempDir::new("src7").unwrap();
        let base = || script_tool(work.path(), "t", "#!/bin/sh\n");
        let key = |item: Injectable| {
            inputs_fingerprint(&config(vec![item]), &mut deps::Resolver::default()).unwrap()
        };
        let plain = key(base());
        // (`copy_deps` too, but turning it on asks ldd about the binary.)
        let changes: [fn(&mut Injectable); 3] = [
            |i| i.skip_if_exists = true,
            |i| i.target_binary = Some("/opt/t".into()),
            |i| i.target_config = Some("~/.t".into()),
        ];
        for change in changes {
            let mut item = base();
            change(&mut item);
            assert_ne!(key(item), plain);
        }
    }

    #[test]
    fn git_settings_are_part_of_the_fingerprint() {
        let a = config(vec![]);
        let mut b = config(vec![]);
        b.git.safe_directories = vec!["/work".into()];
        assert_ne!(
            inputs_fingerprint(&a, &mut deps::Resolver::default()).unwrap(),
            inputs_fingerprint(&b, &mut deps::Resolver::default()).unwrap()
        );
        assert_eq!(
            inputs_fingerprint(&a, &mut deps::Resolver::default()).unwrap(),
            inputs_fingerprint(&config(vec![]), &mut deps::Resolver::default()).unwrap()
        );
    }

    #[test]
    fn a_bundle_file_is_extracted_once_and_then_reused() {
        let work = TempDir::new("src4").unwrap();
        let out = TempDir::new("out4").unwrap();
        let cache = TempDir::new("cache4").unwrap();
        let cfg = config(vec![script_tool(work.path(), "t", "#!/bin/sh\n")]);
        let file = out.path().join("b.tar");
        pack::pack_to_file(&cfg, &file).unwrap();

        // The config used to *inject* has no tools: the bundle supplies them.
        let empty = config(vec![]);
        let first = prepare_in(cache.path(), &empty, Some(&file)).unwrap();
        assert_eq!(first.origin, Origin::Extracted);
        assert_eq!(first.manifest.tools.len(), 1);
        assert!(first.bin("t").exists());
        let second = prepare_in(cache.path(), &empty, Some(&file)).unwrap();
        assert_eq!(second.origin, Origin::Cached);
    }

    #[test]
    fn a_damaged_bundle_file_is_refused_and_leaves_nothing_cached() {
        let work = TempDir::new("src5").unwrap();
        let out = TempDir::new("out5").unwrap();
        let cache = TempDir::new("cache5").unwrap();
        let cfg = config(vec![script_tool(
            work.path(),
            "t",
            "#!/bin/sh\necho data\n",
        )]);
        let file = out.path().join("b.tar");
        pack::pack_to_file(&cfg, &file).unwrap();
        let mut bytes = std::fs::read(&file).unwrap();
        let pos = bytes.windows(4).position(|w| w == b"data").unwrap();
        bytes[pos] ^= 0xFF;
        std::fs::write(&file, bytes).unwrap();

        let err = prepare_in(cache.path(), &config(vec![]), Some(&file))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("damaged"), "{err:#}");
        let leftovers: Vec<_> = std::fs::read_dir(cache.path()).unwrap().collect();
        assert!(
            leftovers.is_empty(),
            "a failed extraction left {} entries",
            leftovers.len()
        );
    }

    #[test]
    fn the_cache_can_be_listed_and_cleared() {
        let tmp = TempDir::new("cache-list").unwrap();
        let root = tmp.path().join("bundles");
        let script = script_tool(tmp.path(), "hello", "echo hi");
        prepare_in(&root, &config(vec![script]), None).unwrap();
        let entries = cache::entries(&root);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].manifest.is_some());
        assert!(entries[0].size > 0);
        let (n, size) = cache::clear(&root);
        assert_eq!((n, size > 0), (1, true));
        assert!(cache::entries(&root).is_empty());
    }

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
            last = Some(prepare_in(cache.path(), &cfg, None).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let kept = std::fs::read_dir(cache.path()).unwrap().count();
        assert_eq!(kept, cache::KEEP);
        assert!(
            last.unwrap().dir.exists(),
            "the bundle in use must survive pruning"
        );
    }
}
