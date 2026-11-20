//! Getting a bundle ready to inject from.
//!
//! Injection reads only from a bundle *directory*: a manifest plus `bin/`, `lib/`
//! and `config/`. This module produces one, from either source, and keeps it in a
//! cache so work is not repeated:
//!
//! - a bundle **file** (`--bundle`, or `bundle = …` in the config) is extracted
//!   once, keyed by the hash of the file;
//! - otherwise the config is **packed live**, keyed by a fingerprint of everything
//!   it depends on (the config entries, each source file's size and mtime, the
//!   libraries `ldd` finds). If nothing changed, the cached bundle is reused and
//!   neither `ldd` nor `patchelf` runs again.
//!
//! So the live path and the bundle-file path end in the same place, and there is a
//! single injector.

use anyhow::{Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::manifest::{BIN_DIR, CONFIG_DIR, FORMAT, LIB_DIR, MANIFEST_FILE, Manifest};
use super::tree::hash_path;
use super::{archive, pack};
use crate::config::Config;
use crate::deps;
use crate::elf::Arch;
use crate::fingerprint::{FNV_OFFSET, fnv1a};
use crate::output::{info, warning};
use crate::tmpfile::{TempDir, discard_dir};

/// How many cached bundles to keep (the most recently used).
const KEEP: usize = 4;

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
    let root = dirs::cache_dir()
        .context("this system has no cache directory")?
        .join("lodge")
        .join("bundles");
    prepare_in(&root, config, file)
}

pub(crate) fn prepare_in(root: &Path, config: &Config, file: Option<&Path>) -> Result<Prepared> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
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
    prune(root, &prepared.dir);

    let how = match prepared.origin {
        Origin::Cached => "cached",
        Origin::Packed => "packed",
        Origin::Extracted => "extracted",
    };
    info!("bundle {} ({how})", prepared.manifest.id());
    Ok(prepared)
}

fn from_file(root: &Path, file: &Path) -> Result<Prepared> {
    let key = format!("f-{}", hash_path(file)?);
    let dir = root.join(&key);
    if let Some(manifest) = load_cached(&dir) {
        return Ok(Prepared {
            dir,
            manifest,
            origin: Origin::Cached,
        });
    }
    let staging = TempDir::new_in(root, "extract")?;
    let manifest = archive::extract(file, staging.path())?;
    let dir = install(staging, &dir)?;
    Ok(Prepared {
        dir,
        manifest,
        origin: Origin::Extracted,
    })
}

fn live(root: &Path, config: &Config) -> Result<Prepared> {
    let key = format!("p-{}", inputs_fingerprint(config)?);
    let dir = root.join(&key);
    if let Some(manifest) = load_cached(&dir) {
        return Ok(Prepared {
            dir,
            manifest,
            origin: Origin::Cached,
        });
    }
    let staging = TempDir::new_in(root, "pack")?;
    let manifest = pack::build(config, staging.path())?;
    let dir = install(staging, &dir)?;
    Ok(Prepared {
        dir,
        manifest,
        origin: Origin::Packed,
    })
}

/// The cached manifest in `dir`, marking the entry as recently used; `None` if
/// there is no valid bundle there.
fn load_cached(dir: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(dir.join(MANIFEST_FILE)).ok()?;
    let manifest = Manifest::from_json(&bytes).ok()?;
    touch(dir);
    Some(manifest)
}

/// Moves a finished staging directory into place. Atomic, so a cache entry is
/// either complete or absent. If a concurrent run got there first its copy is
/// just as good.
fn install(staging: TempDir, dir: &Path) -> Result<PathBuf> {
    match std::fs::rename(staging.path(), dir) {
        Ok(()) => Ok(dir.to_path_buf()),
        Err(_) if dir.join(MANIFEST_FILE).exists() => Ok(dir.to_path_buf()),
        Err(e) => Err(e).with_context(|| format!("saving the bundle to {}", dir.display())),
    }
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "recency is only a pruning hint"
)]
fn touch(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.set_modified(SystemTime::now());
    }
}

/// Removes the least recently used cache entries beyond [`KEEP`], and staging
/// directories left behind by a crashed run. `keep` is never removed.
fn prune(root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut cached = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified = entry.metadata().and_then(|m| m.modified()).ok();
        if name.starts_with("f-") || name.starts_with("p-") {
            cached.push((modified, entry.path()));
        } else if name.starts_with("lodge-")
            && modified
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 3600)
        {
            discard_dir(&entry.path());
        }
    }
    cached.sort_by_key(|entry| std::cmp::Reverse(entry.0)); // newest first
    for (_, path) in cached.into_iter().skip(KEEP) {
        if path != keep {
            discard_dir(&path);
        }
    }
}

/// A fingerprint of everything a live pack depends on. Packing is skipped when it
/// is unchanged.
pub(crate) fn inputs_fingerprint(config: &Config) -> Result<String> {
    let mut h = FNV_OFFSET;
    let mut put = |data: &str| {
        h = fnv1a(data.as_bytes(), h);
        h = fnv1a(&[0], h);
    };
    put(env!("CARGO_PKG_VERSION"));
    put(&FORMAT.to_string());
    put(&Arch::from_uname(std::env::consts::ARCH).map_or_else(String::new, |a| a.to_string()));

    let mut parts = Vec::new();
    for item in &config.inject {
        parts.push(format!(
            "tool {} deps={} skip={} bin={:?} cfg={:?}",
            item.name, item.copy_deps, item.skip_if_exists, item.target_binary, item.target_config
        ));
        if let Some(binary) = &item.binary {
            // Before ldd, which cannot inspect a binary for another architecture.
            pack::check_arch(&item.name, binary, pack::host_arch()?)?;
            parts.push(stat_line(binary)?);
            if item.copy_deps {
                let deps = deps::resolve(&binary.to_string_lossy())?;
                for lib in &deps.libs {
                    parts.push(stat_line(lib)?);
                }
            }
        }
        if let Some(cfg) = &item.config {
            stat_tree(cfg, &mut parts)?;
        }
    }
    let mut aliases: Vec<_> = config.aliases.iter().collect();
    aliases.sort();
    parts.push(format!("aliases {aliases:?}"));
    parts.push(format!("git {:?}", config.git.safe_directories));

    for part in &parts {
        put(part);
    }
    Ok(format!("{h:016x}"))
}

/// `path size mtime mode`, following symlinks to the file that is really used.
fn stat_line(path: &Path) -> Result<String> {
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
    let real = std::fs::canonicalize(root)
        .with_context(|| format!("config path {} does not exist", root.display()))?;
    walk(&real, &real, out)
}

fn walk(root: &Path, path: &Path, out: &mut Vec<String>) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    if meta.is_symlink() {
        out.push(format!(
            "L {relative} -> {}",
            std::fs::read_link(path)?.display()
        ));
    } else if meta.is_dir() {
        out.push(format!("D {relative} {:o}", meta.mode()));
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            walk(root, &entry.path(), out)?;
        }
    } else {
        out.push(format!(
            "F {relative} {} {}.{} {:o}",
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.mode()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Injectable;

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
            ..Config::for_tests()
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
    fn aliases_are_part_of_the_fingerprint() {
        let a = config(vec![]);
        let mut b = config(vec![]);
        b.aliases.insert("ll".into(), "ls -la".into());
        assert_ne!(
            inputs_fingerprint(&a).unwrap(),
            inputs_fingerprint(&b).unwrap()
        );
        assert_eq!(
            inputs_fingerprint(&a).unwrap(),
            inputs_fingerprint(&config(vec![])).unwrap()
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
    fn only_the_most_recently_used_bundles_are_kept() {
        let work = TempDir::new("src6").unwrap();
        let cache = TempDir::new("cache6").unwrap();
        let mut last = None;
        for i in 0..(KEEP + 2) {
            let cfg = config(vec![script_tool(
                work.path(),
                "t",
                &format!("#!/bin/sh\necho {i}{}\n", "x".repeat(i)),
            )]);
            last = Some(prepare_in(cache.path(), &cfg, None).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let kept = std::fs::read_dir(cache.path()).unwrap().count();
        assert_eq!(kept, KEEP);
        assert!(
            last.unwrap().dir.exists(),
            "the bundle in use must survive pruning"
        );
    }
}
