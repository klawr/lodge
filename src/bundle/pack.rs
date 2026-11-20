//! Building a bundle from the user's config: the only step that needs `ldd` and
//! `patchelf`, and the only one that reads the host's own binaries.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::archive;
use super::deps;
use super::elf::{self, Arch};
use super::hash::hash_path;
use super::manifest::{
    BIN_DIR, Binary, CONFIG_DIR, ConfigTree, FORMAT, LIB_DIR, Lib, MANIFEST_FILE, Manifest, Tool,
};
use crate::config::{Config, Injectable};
use crate::layout;
use crate::output::warning;
use crate::shell::is_plain_name;
use crate::tmpfile::{TempDir, TempFile, discard_file};

/// Packs `config` into a bundle file at `output`. The file appears only when it is
/// complete: it is written beside the destination and renamed into place.
pub fn pack_to_file(config: &Config, output: &Path) -> Result<Manifest> {
    let scratch = TempDir::new("pack")?;
    let manifest = build(config, scratch.path())?;

    let partial = PathBuf::from(format!("{}.partial", output.display()));
    let written = std::fs::File::create(&partial)
        .with_context(|| format!("creating {}", partial.display()))
        .and_then(|file| archive::write_tar(scratch.path(), file))
        .and_then(|()| {
            std::fs::rename(&partial, output)
                .with_context(|| format!("moving the bundle to {}", output.display()))
        });
    if written.is_err() {
        discard_file(&partial);
    }
    written?;
    Ok(manifest)
}

/// Builds the bundle's directory layout and manifest in `dir`.
pub fn build(config: &Config, dir: &Path) -> Result<Manifest> {
    let host = host_arch()?;
    for sub in [BIN_DIR, LIB_DIR, CONFIG_DIR] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let mut packer = Packer {
        dir,
        host,
        libs: BTreeMap::new(),
        linker: None,
        tools: Vec::new(),
    };
    for item in &config.inject {
        packer.add(item)?;
    }
    let manifest = packer.finish(config);
    manifest
        .validate()
        .context("the bundle would not be valid")?;
    std::fs::write(dir.join(MANIFEST_FILE), manifest.to_json()?)?;
    Ok(manifest)
}

struct Packer<'a> {
    dir: &'a Path,
    host: Arch,
    /// Bundled libraries: file name to content hash.
    libs: BTreeMap<String, String>,
    linker: Option<String>,
    tools: Vec<Tool>,
}

impl Packer<'_> {
    fn add(&mut self, item: &Injectable) -> Result<()> {
        // `Config::parse` checks this too; a `Config` built in code may not
        // have been, and the name becomes a path in the bundle.
        if !is_plain_name(&item.name) {
            bail!(
                "'{}' is not usable as a tool name (letters, digits and -_.+ only)",
                item.name
            );
        }
        let binary = item
            .binary
            .as_deref()
            .map(|b| self.add_binary(item, b))
            .transpose()?;
        let config = item
            .config
            .as_deref()
            .map(|c| self.add_config(item, c))
            .transpose()?;
        self.tools.push(Tool {
            name: item.name.clone(),
            binary,
            config,
            skip_if_exists: item.skip_if_exists,
        });
        Ok(())
    }

    fn add_binary(&mut self, item: &Injectable, source: &Path) -> Result<Binary> {
        let name = &item.name;
        let path = source.to_string_lossy();
        check_arch(name, source, self.host)?;

        let dst = self.dir.join(BIN_DIR).join(name);
        let mut dynamic = false;
        if item.copy_deps {
            let deps = deps::resolve(&path)?;
            if !deps.is_empty() {
                let linker = deps
                    .linker
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .with_context(|| {
                        format!("{name} is dynamic but ldd named no dynamic linker")
                    })?;
                self.add_libs(&deps.libs, &linker, name)?;
                let patched = patch_binary(&path, &linker)?;
                std::fs::copy(patched.path(), &dst)
                    .with_context(|| format!("copying the relinked {name}"))?;
                dynamic = true;
            }
        }
        if !dynamic {
            let real = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
            std::fs::copy(&real, &dst).with_context(|| format!("copying {}", real.display()))?;
        }
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;

        Ok(Binary {
            target: item
                .target_binary
                .clone()
                .unwrap_or_else(|| format!("/usr/local/bin/{name}")),
            dynamic,
            hash: hash_path(&dst)?,
        })
    }

    /// Adds a dynamic binary's libraries to the bundle. A bundle holds exactly one
    /// glibc and one copy of each library, so two tools that need different builds
    /// of the same library are refused instead of one silently winning.
    fn add_libs(&mut self, libs: &[PathBuf], linker: &str, tool: &str) -> Result<()> {
        match &self.linker {
            Some(existing) if existing != linker => bail!(
                "{tool} uses the dynamic linker {linker} but another tool uses {existing}; \
                 a bundle holds one glibc"
            ),
            _ => self.linker = Some(linker.to_string()),
        }
        for lib in libs {
            let Some(file_name) = lib.file_name() else {
                continue;
            };
            let name = file_name.to_string_lossy().into_owned();
            let real = std::fs::canonicalize(lib).unwrap_or_else(|_| lib.clone());
            let hash = hash_path(&real)?;
            match self.libs.get(&name) {
                Some(existing) if *existing == hash => {}
                Some(_) => bail!(
                    "{tool} needs a different build of {name} than a tool packed earlier; \
                     a bundle holds one copy of each library"
                ),
                None => {
                    std::fs::copy(&real, self.dir.join(LIB_DIR).join(&name))
                        .with_context(|| format!("copying {}", real.display()))?;
                    self.libs.insert(name, hash);
                }
            }
        }
        Ok(())
    }

    fn add_config(&mut self, item: &Injectable, source: &Path) -> Result<ConfigTree> {
        let name = &item.name;
        if let Some(why) = sensitive_reason(source, dirs::home_dir().as_deref()) {
            warning!(
                "{name}: {} may contain {why}, and a bundle is meant to be shared",
                source.display()
            );
        }
        let real = std::fs::canonicalize(source)
            .with_context(|| format!("{name}: config path {} does not exist", source.display()))?;
        let dst = self.dir.join(CONFIG_DIR).join(name);
        copy_tree(&real, &dst)?;
        Ok(ConfigTree {
            target: item
                .target_config
                .clone()
                .unwrap_or_else(|| format!("~/.config/{name}")),
            is_dir: real.is_dir(),
            hash: hash_path(&dst)?,
        })
    }

    fn finish(self, config: &Config) -> Manifest {
        Manifest {
            format: FORMAT,
            lodge_version: env!("CARGO_PKG_VERSION").to_string(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            arch: self.host.to_string(),
            linker: self.linker,
            libs: self
                .libs
                .into_iter()
                .map(|(name, hash)| Lib { name, hash })
                .collect(),
            tools: self.tools,
            aliases: config
                .aliases
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            git_safe_directories: config.git.safe_directories.clone(),
        }
    }
}

/// This machine's CPU architecture.
pub(crate) fn host_arch() -> Result<Arch> {
    Arch::from_uname(std::env::consts::ARCH).context("unsupported host architecture")
}

/// Refuses a binary built for another CPU than this machine's: it cannot be
/// inspected with `ldd` here, and a bundle holds one architecture.
pub(crate) fn check_arch(tool: &str, binary: &Path, host: Arch) -> Result<()> {
    if let Some(arch) = elf::arch_of(binary)?
        && arch != host
    {
        bail!(
            "{tool}: {} is built for {arch}, but this machine is {host}; \
             pack on a {arch} machine",
            binary.display()
        );
    }
    Ok(())
}

/// Copies a file or directory tree, keeping symlinks as symlinks and modes as they
/// are.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if meta.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
            .with_context(|| format!("copying the link {}", src.display()))?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        let mut entries: Vec<_> = std::fs::read_dir(src)
            .with_context(|| format!("reading {}", src.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, meta.permissions())?;
    } else {
        std::fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

/// What `path` probably holds, if it is somewhere credentials usually live.
fn sensitive_reason(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    const PLACES: &[(&str, &str)] = &[
        (".ssh", "SSH keys"),
        (".gnupg", "GPG keys"),
        (".aws", "AWS credentials"),
        (".config/gh", "GitHub CLI credentials"),
        (".claude", "Claude credentials"),
        (".netrc", "login credentials"),
        (".docker/config.json", "registry credentials"),
    ];
    let home = home?;
    PLACES
        .iter()
        .find(|(relative, _)| path.starts_with(home.join(relative)))
        .map(|(_, why)| *why)
}

/// Relinks a copy of `binary` so it runs *directly* against the bundled glibc,
/// with `patchelf`:
///   --set-interpreter /lodge/<ld> : the bundled dynamic linker is the program
///                                   interpreter
///   --force-rpath --set-rpath …   : DT_RPATH (not RUNPATH) so the search path
///                                   also resolves *transitive* deps (e.g. libnsl
///                                   pulling libtirpc); RUNPATH would not.
///
/// Running directly keeps `/proc/self/exe` pointing at the binary, which tools
/// like Neovim need when they re-exec themselves (`--embed`); baking the search
/// path into the binary (rather than exporting `LD_LIBRARY_PATH`) keeps the
/// subprocesses it spawns (git, shells, LSPs) on the container's own glibc.
/// patchelf copes even when there is no slack in the original `.dynstr` and
/// `.dynamic`, which is the common case for real binaries.
pub(crate) fn patch_binary(binary: &str, linker_name: &str) -> Result<TempFile> {
    let data = std::fs::read(binary).with_context(|| format!("reading {binary} to patch"))?;
    let stem = Path::new(binary).file_name().map_or_else(
        || "binary".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = TempFile::write(&format!("lodge-patched-{stem}"), &data)?;

    let mut cmd = crate::host_tools::PATCHELF.command();
    cmd.arg("--set-interpreter")
        .arg(format!("{}/{linker_name}", layout::LINKER_DIR))
        .arg("--force-rpath")
        .arg("--set-rpath")
        .arg(layout::LIB)
        .arg(tmp.path());
    crate::output::trace(&cmd);
    let status = cmd.status().context(
        "running patchelf — it is required to relink dynamically linked binaries \
         (install it, e.g. `pacman -S patchelf` / `apt install patchelf`)",
    )?;
    if !status.success() {
        bail!("patchelf failed to patch {binary}");
    }
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Injectable;

    fn script_tool(dir: &Path, name: &str) -> Injectable {
        let bin = dir.join(format!("{name}-src"));
        std::fs::write(&bin, "#!/bin/sh\necho hi\n").unwrap();
        Injectable {
            name: name.into(),
            binary: Some(bin),
            config: None,
            target_binary: None,
            target_config: None,
            skip_if_exists: true,
            // A script has no libraries, and this keeps the test free of ldd and
            // patchelf.
            copy_deps: false,
        }
    }

    fn config_with(inject: Vec<Injectable>) -> Config {
        Config {
            inject,
            ..Config::default()
        }
    }

    #[test]
    fn a_tool_and_its_config_become_a_verifiable_bundle() {
        let work = TempDir::new("pack-src").unwrap();
        let mut tool = script_tool(work.path(), "hello");
        let cfg = work.path().join("hello-config");
        std::fs::create_dir_all(cfg.join("sub")).unwrap();
        std::fs::write(cfg.join("sub/rc"), "set x").unwrap();
        std::os::unix::fs::symlink("sub/rc", cfg.join("link")).unwrap();
        tool.config = Some(cfg);
        tool.target_config = Some("~/.config/hello".into());

        let out = TempDir::new("pack-out").unwrap();
        let file = out.path().join("b.tar");
        let manifest = pack_to_file(&config_with(vec![tool]), &file).unwrap();

        let tool = &manifest.tools[0];
        assert_eq!(tool.name, "hello");
        assert_eq!(tool.binary.as_ref().unwrap().target, "/usr/local/bin/hello");
        assert!(!tool.binary.as_ref().unwrap().dynamic);
        assert!(tool.config.as_ref().unwrap().is_dir);
        assert_eq!(
            manifest.arch,
            Arch::from_uname(std::env::consts::ARCH)
                .unwrap()
                .to_string()
        );

        let dest = TempDir::new("pack-dest").unwrap();
        assert_eq!(archive::extract(&file, dest.path()).unwrap(), manifest);
        assert_eq!(
            std::fs::read_link(dest.path().join("config/hello/link")).unwrap(),
            Path::new("sub/rc"),
            "symlinks stay symlinks"
        );
        assert!(!out.path().join("b.tar.partial").exists());
    }

    #[test]
    fn packing_the_same_inputs_twice_gives_the_same_id() {
        let work = TempDir::new("pack-id").unwrap();
        let tool = script_tool(work.path(), "t");
        let cfg = config_with(vec![tool]);
        let a = TempDir::new("id-a").unwrap();
        let b = TempDir::new("id-b").unwrap();
        let ma = build(&cfg, a.path()).unwrap();
        let mb = build(&cfg, b.path()).unwrap();
        assert_eq!(ma.id(), mb.id());
    }

    #[test]
    fn a_tool_name_that_could_escape_the_bundle_is_refused() {
        let work = TempDir::new("pack-bad").unwrap();
        let tool = script_tool(work.path(), "../evil");
        let out = TempDir::new("pack-bad-out").unwrap();
        let err = build(&config_with(vec![tool]), out.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not usable as a tool name"), "{err}");
    }

    #[test]
    fn a_missing_config_path_names_the_tool() {
        let work = TempDir::new("pack-missing").unwrap();
        let mut tool = script_tool(work.path(), "t");
        tool.config = Some(work.path().join("nope"));
        let out = TempDir::new("pack-missing-out").unwrap();
        let err = format!(
            "{:#}",
            build(&config_with(vec![tool]), out.path()).unwrap_err()
        );
        assert!(
            err.contains("t: config path") && err.contains("does not exist"),
            "{err}"
        );
    }

    #[test]
    fn a_binary_for_another_architecture_is_refused() {
        let work = TempDir::new("pack-arch").unwrap();
        let mut tool = script_tool(work.path(), "foreign");
        // An ELF header for the architecture this machine is *not*.
        let machine: [u8; 2] = if Arch::from_uname(std::env::consts::ARCH) == Some(Arch::Aarch64) {
            [0x3E, 0]
        } else {
            [0xB7, 0]
        };
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[5] = 1;
        elf[18..20].copy_from_slice(&machine);
        let path = work.path().join("foreign-bin");
        std::fs::write(&path, elf).unwrap();
        tool.binary = Some(path);
        let out = TempDir::new("pack-arch-out").unwrap();
        let err = build(&config_with(vec![tool]), out.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("pack on a"), "{err}");
    }

    #[test]
    fn credentials_directories_are_recognised() {
        let home = Path::new("/home/me");
        assert_eq!(
            sensitive_reason(Path::new("/home/me/.ssh/config"), Some(home)),
            Some("SSH keys")
        );
        assert_eq!(
            sensitive_reason(Path::new("/home/me/.config/gh"), Some(home)),
            Some("GitHub CLI credentials")
        );
        assert_eq!(
            sensitive_reason(Path::new("/home/me/.config/nvim"), Some(home)),
            None
        );
        assert_eq!(sensitive_reason(Path::new("/home/me/.ssh"), None), None);
    }
}
