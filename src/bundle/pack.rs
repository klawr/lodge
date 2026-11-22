//! Building a bundle from the user's config: the only step that needs `ldd` and
//! `patchelf`, and the only one that reads the host's own binaries.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::archive;
use super::deps;
use super::elf;
use super::hash::hash_path;
use super::manifest::{
    BIN_DIR, Binary, DATA_DIR, Data, LIB_DIR, Lib, MANIFEST_FILE, Manifest, ORIGIN_RPATH, data_file,
};
use crate::arch::Arch;
use crate::config::{BinaryEntry, Config, DataEntry};
use crate::output::warning;
use crate::shell::is_plain_name;
use crate::tmpfile::{TempDir, TempFile, discard_file};
use crate::tools::{ElfEditor, Toolchain};
use crate::tree::{self, Denied, Links};

/// Packs `config` into a bundle file at `output`. The file appears only when it is
/// complete: it is written beside the destination and renamed into place.
pub fn pack_to_file(config: &Config, output: &Path, tools: &Toolchain) -> Result<Manifest> {
    let packer = tools.packer()?;
    build_tar(
        config,
        output,
        &mut deps::Resolver::new(packer.deps.as_ref()),
        packer.elf.as_ref(),
    )
}

/// [`build`] in a scratch directory, then written whole as a tar at `output`
/// (see [`pack_to_file`]).
pub fn build_tar(
    config: &Config,
    output: &Path,
    deps: &mut deps::Resolver<'_>,
    elf: &dyn ElfEditor,
) -> Result<Manifest> {
    let scratch = TempDir::new("pack")?;
    let manifest = build(config, scratch.path(), deps, elf)?;

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

/// Builds the bundle's directory layout and manifest in `dir`. `deps` answers
/// which libraries a binary needs (and remembers what it was asked before), and
/// `elf` relinks the binaries that have some.
pub fn build(
    config: &Config,
    dir: &Path,
    deps: &mut deps::Resolver<'_>,
    elf: &dyn ElfEditor,
) -> Result<Manifest> {
    let host = Arch::host()?;
    for sub in [BIN_DIR, LIB_DIR, DATA_DIR] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let mut packer = Packer {
        dir,
        deps,
        editor: elf,
        host,
        home: crate::config::home_dir(),
        libs: BTreeMap::new(),
        linker: None,
        binaries: Vec::new(),
        data: Vec::new(),
    };
    for item in &config.binaries {
        packer.add_binary(item)?;
    }
    for item in &config.data {
        packer.add_data(item)?;
    }
    let manifest = packer.finish(config);
    manifest
        .validate()
        .context("the bundle would not be valid")?;
    std::fs::write(dir.join(MANIFEST_FILE), manifest.to_json()?)?;
    Ok(manifest)
}

struct Packer<'a, 'r> {
    dir: &'a Path,
    deps: &'a mut deps::Resolver<'r>,
    editor: &'a dyn ElfEditor,
    host: Arch,
    /// The home directory data under it is relative to.
    home: PathBuf,
    /// Bundled libraries: file name to content hash.
    libs: BTreeMap<String, String>,
    linker: Option<String>,
    binaries: Vec<Binary>,
    data: Vec<Data>,
}

impl Packer<'_, '_> {
    fn add_binary(&mut self, item: &BinaryEntry) -> Result<()> {
        // `Config::parse` checks this too; a `Config` built in code may not
        // have been, and the name becomes a path in the bundle.
        if !is_plain_name(&item.name) {
            bail!(
                "'{}' is not usable as a name (letters, digits and -_.+ only)",
                item.name
            );
        }
        let binary = self.pack_binary(item)?;
        self.binaries.push(binary);
        Ok(())
    }

    fn pack_binary(&mut self, item: &BinaryEntry) -> Result<Binary> {
        let name = &item.name;
        let source = item.path.as_path();
        let path = source.to_string_lossy();
        elf::check_arch(name, source, self.host)?;

        let dst = self.dir.join(BIN_DIR).join(name);
        let mut dynamic = false;
        if item.copy_deps {
            let deps = self.deps.resolve(&path)?;
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
                let patched = patch_binary(&path, self.editor)?;
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
            name: name.clone(),
            dest: item.target.clone(),
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

    /// Copies a file or directory into the bundle, as the next `data/<n>`.
    fn add_data(&mut self, item: &DataEntry) -> Result<()> {
        let source = item.path.as_path();
        if let Some(why) = sensitive_reason(source, dirs::home_dir().as_deref()) {
            warning!(
                "{}: may contain {why}, and a bundle is meant to be shared",
                source.display()
            );
        }
        let real = std::fs::canonicalize(source)
            .with_context(|| format!("data path {} does not exist", source.display()))?;
        let dst = self.dir.join(data_file(self.data.len()));
        copy_tree(&real, &dst)?;
        self.data.push(Data {
            dest: item.dest(&self.home),
            is_dir: real.is_dir(),
            hash: hash_path(&dst)?,
            binary: item.binary.clone(),
        });
        Ok(())
    }

    fn finish(self, config: &Config) -> Manifest {
        Manifest {
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
            binaries: self.binaries,
            data: self.data,
            git_safe_directories: config.git.safe_directories.clone(),
        }
    }
}

/// Copies a file or directory tree, keeping symlinks as symlinks and modes as they
/// are.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    // A directory's own mode is applied once everything is in it, so a
    // read-only directory can still be filled.
    let mut dir_modes = Vec::new();
    tree::walk(src, Links::Keep, Denied::Fail, &mut |e| {
        // The root itself is `dst`: `join("")` would make it `dst/`, which a
        // single file cannot be copied to.
        let to = if e.relative.is_empty() {
            dst.to_path_buf()
        } else {
            dst.join(e.relative)
        };
        if e.meta.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(e.path)?, &to)
                .with_context(|| format!("copying the link {}", e.path.display()))?;
        } else if e.meta.is_dir() {
            std::fs::create_dir_all(&to)?;
            dir_modes.push((to, e.meta.permissions()));
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(e.path, &to).with_context(|| format!("copying {}", e.path.display()))?;
        }
        Ok(())
    })?;
    for (dir, mode) in dir_modes.into_iter().rev() {
        std::fs::set_permissions(dir, mode)?;
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

/// Relinks a copy of `binary` so that, once injected, it runs *directly* against
/// the bundled glibc, with an [`ElfEditor`] (`patchelf`, today):
///
/// - `--set-interpreter <placeholder>`: a path that fills the space reserved for
///   the interpreter ([`elf::INTERP_CAPACITY`]); injection overwrites it in place
///   with the bundled dynamic linker's real path, so the bundle is tied to no
///   install prefix. (`patchelf` is the one that has to grow the field; the
///   rewrite at injection never moves anything.)
/// - `--force-rpath --set-rpath $ORIGIN/../lib`: DT_RPATH (not RUNPATH), so the
///   search path also resolves *transitive* deps (e.g. libnsl pulling libtirpc);
///   RUNPATH would not. It is relative to the binary, so it needs no rewrite.
///
/// Running directly keeps `/proc/self/exe` pointing at the binary, which tools
/// like Neovim need when they re-exec themselves (`--embed`); baking the search
/// path into the binary (rather than exporting `LD_LIBRARY_PATH`) keeps the
/// subprocesses it spawns (git, shells, LSPs) on the container's own glibc.
/// patchelf copes even when there is no slack in the original `.dynstr` and
/// `.dynamic`, which is the common case for real binaries.
pub(crate) fn patch_binary(binary: &str, editor: &dyn ElfEditor) -> Result<TempFile> {
    let data = std::fs::read(binary).with_context(|| format!("reading {binary} to patch"))?;
    let stem = Path::new(binary).file_name().map_or_else(
        || "binary".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = TempFile::write(&format!("lodge-patched-{stem}"), &data)?;
    editor
        .relink(tmp.path(), &elf::placeholder_interpreter(), ORIGIN_RPATH)
        .with_context(|| format!("relinking {binary}"))?;
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::reader::Bundle;
    use crate::config::{BinaryEntry, DataEntry};
    use crate::snapshot::{Kind, Snapshot};
    use crate::tools::{self, Ldd, Patchelf};
    use std::io::Read as _;

    fn script_tool(dir: &Path, name: &str) -> BinaryEntry {
        let bin = dir.join(format!("{name}-src"));
        std::fs::write(&bin, "#!/bin/sh\necho hi\n").unwrap();
        BinaryEntry {
            name: name.into(),
            path: bin,
            target: None,
            // A script has no libraries, and this keeps the test free of ldd and
            // patchelf.
            copy_deps: false,
        }
    }

    fn config_with(binaries: Vec<BinaryEntry>) -> Config {
        Config {
            binaries,
            ..Config::default()
        }
    }

    fn data_entry(path: PathBuf, target: Option<&str>, binary: Option<&str>) -> DataEntry {
        DataEntry {
            path,
            target: target.map(String::from),
            binary: binary.map(String::from),
        }
    }

    #[test]
    fn a_binary_and_its_data_become_a_verifiable_bundle() {
        let work = TempDir::new("pack-src").unwrap();
        let tool = script_tool(work.path(), "hello");
        let cfg = work.path().join("hello-config");
        std::fs::create_dir_all(cfg.join("sub")).unwrap();
        std::fs::write(cfg.join("sub/rc"), "set x").unwrap();
        std::os::unix::fs::symlink("sub/rc", cfg.join("link")).unwrap();
        let single = work.path().join("one");
        std::fs::write(&single, "1").unwrap();
        let mut config = config_with(vec![tool]);
        config
            .data
            .push(data_entry(cfg, Some("~/.config/hello"), Some("hello")));
        config.data.push(data_entry(single, Some("/etc/one"), None));

        let out = TempDir::new("pack-out").unwrap();
        let file = out.path().join("b.tar");
        let manifest = pack_to_file(&config, &file, &tools::external()).unwrap();

        assert_eq!(manifest.binaries.len(), 1);
        assert_eq!(manifest.binaries[0].name, "hello");
        assert_eq!(manifest.binaries[0].dest, None, "no wrapper asked for");
        assert!(!manifest.binaries[0].dynamic);
        assert_eq!(manifest.data.len(), 2);
        assert_eq!(manifest.data[0].dest, "~/.config/hello");
        assert!(manifest.data[0].is_dir);
        assert_eq!(manifest.data[0].binary.as_deref(), Some("hello"));
        assert!(!manifest.data[1].is_dir);
        assert_eq!(manifest.data[1].binary, None);
        assert_eq!(manifest.arch, Arch::host().unwrap().to_string());

        let bundle = Bundle::verified(&file).unwrap();
        assert_eq!(bundle.manifest(), &manifest);
        let mut links = Vec::new();
        bundle
            .data(0)
            .walk(&mut |n| {
                if let Kind::Link(to) = &n.kind {
                    links.push((n.relative.clone(), to.clone()));
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            links,
            [("link".to_string(), "sub/rc".to_string())],
            "symlinks stay symlinks"
        );
        let mut single = String::new();
        bundle
            .data(1)
            .open("")
            .unwrap()
            .read_to_string(&mut single)
            .unwrap();
        assert_eq!(single, "1");
        assert!(!out.path().join("b.tar.partial").exists());
    }

    #[test]
    fn data_without_a_target_is_for_the_place_it_is_at() {
        let work = TempDir::new("pack-mirror").unwrap();
        let dir = work.path().join("share/hello");
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = config_with(vec![]);
        config.data.push(data_entry(dir.clone(), None, None));
        let out = TempDir::new("pack-mirror-out").unwrap();
        let manifest = build(
            &config,
            out.path(),
            &mut deps::Resolver::new(&Ldd),
            &Patchelf,
        )
        .unwrap();
        assert_eq!(manifest.data[0].dest, dir.display().to_string());
    }

    #[test]
    fn two_data_entries_for_one_place_are_refused() {
        let work = TempDir::new("pack-twice").unwrap();
        let dir = work.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = config_with(vec![]);
        config.data.push(data_entry(dir.clone(), None, None));
        config.data.push(data_entry(dir, None, None));
        let out = TempDir::new("pack-twice-out").unwrap();
        let err = format!(
            "{:#}",
            build(
                &config,
                out.path(),
                &mut deps::Resolver::new(&Ldd),
                &Patchelf
            )
            .unwrap_err()
        );
        assert!(err.contains("listed twice"), "{err}");
    }

    #[test]
    fn a_wrapper_target_is_kept_only_when_the_config_names_one() {
        let work = TempDir::new("pack-target").unwrap();
        let mut tool = script_tool(work.path(), "hello");
        tool.target = Some("~/.local/bin/hello".into());
        let out = TempDir::new("pack-target-out").unwrap();
        let manifest = build(
            &config_with(vec![tool]),
            out.path(),
            &mut deps::Resolver::new(&Ldd),
            &Patchelf,
        )
        .unwrap();
        assert_eq!(
            manifest.binaries[0].dest.as_deref(),
            Some("~/.local/bin/hello")
        );
    }

    /// An editor that only records what it was asked to do.
    struct Recording(std::cell::RefCell<Vec<(PathBuf, String, String)>>);

    impl ElfEditor for Recording {
        fn relink(&self, file: &Path, interpreter: &str, rpath: &str) -> Result<()> {
            self.0
                .borrow_mut()
                .push((file.to_path_buf(), interpreter.into(), rpath.into()));
            Ok(())
        }
    }

    #[test]
    fn a_binary_is_relinked_as_a_copy_with_the_placeholder_interpreter_and_origin_rpath() {
        let dir = TempDir::new("relink").unwrap();
        let binary = dir.path().join("tool");
        std::fs::write(&binary, "original").unwrap();
        let editor = Recording(Default::default());
        let copy = patch_binary(&binary.to_string_lossy(), &editor).unwrap();
        let calls = editor.0.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, copy.path());
        assert_ne!(calls[0].0, binary, "the original is never edited");
        assert_eq!(calls[0].1, elf::placeholder_interpreter());
        assert_eq!(calls[0].2, ORIGIN_RPATH);
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), "original");
    }

    #[test]
    fn an_editor_that_fails_fails_the_pack_naming_the_binary() {
        struct Failing;
        impl ElfEditor for Failing {
            fn relink(&self, _: &Path, _: &str, _: &str) -> Result<()> {
                bail!("no editor here")
            }
        }
        let dir = TempDir::new("relink-fail").unwrap();
        let binary = dir.path().join("tool");
        std::fs::write(&binary, "x").unwrap();
        let err = patch_binary(&binary.to_string_lossy(), &Failing)
            .err()
            .unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("tool") && text.contains("no editor here"),
            "{text}"
        );
    }

    #[test]
    fn a_dynamic_binary_is_packed_without_an_install_prefix() {
        if crate::host_tools::PATCHELF.find().is_none() || crate::host_tools::LDD.find().is_none() {
            return; // `make check` machines have both; this keeps the test portable
        }
        let work = TempDir::new("pack-dyn").unwrap();
        let mut tool = script_tool(work.path(), "sh");
        let Ok(sh) = std::fs::canonicalize("/bin/sh") else {
            return;
        };
        if deps::Resolver::new(&Ldd)
            .resolve(&sh.to_string_lossy())
            .is_ok_and(|d| d.is_empty())
        {
            return; // a static shell has nothing to relink
        }
        tool.path = sh;
        tool.copy_deps = true;
        let out = TempDir::new("pack-dyn-out").unwrap();
        let manifest = build(
            &config_with(vec![tool]),
            out.path(),
            &mut deps::Resolver::new(&Ldd),
            &Patchelf,
        )
        .unwrap();
        assert!(manifest.binaries[0].dynamic);

        let mut packed = std::fs::read(out.path().join("bin/sh")).unwrap();
        assert_eq!(
            elf::interpreter(&packed).unwrap(),
            Some(elf::placeholder_interpreter()),
            "the interpreter is a placeholder, not a path in some prefix"
        );
        // It can be given any real path up to the reserved size, in place.
        let before = packed.len();
        elf::set_interpreter(&mut packed, "/home/dev/.local/share/lodge/lib/ld.so").unwrap();
        assert_eq!(packed.len(), before);
        let rpath = std::process::Command::new("patchelf")
            .arg("--print-rpath")
            .arg(out.path().join("bin/sh"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&rpath.stdout).trim(), ORIGIN_RPATH);
    }

    #[test]
    fn packing_the_same_inputs_twice_gives_the_same_id() {
        let work = TempDir::new("pack-id").unwrap();
        let tool = script_tool(work.path(), "t");
        let cfg = config_with(vec![tool]);
        let a = TempDir::new("id-a").unwrap();
        let b = TempDir::new("id-b").unwrap();
        let ma = build(&cfg, a.path(), &mut deps::Resolver::new(&Ldd), &Patchelf).unwrap();
        let mb = build(&cfg, b.path(), &mut deps::Resolver::new(&Ldd), &Patchelf).unwrap();
        assert_eq!(ma.id(), mb.id());
    }

    #[test]
    fn a_tool_name_that_could_escape_the_bundle_is_refused() {
        let work = TempDir::new("pack-bad").unwrap();
        let tool = script_tool(work.path(), "../evil");
        let out = TempDir::new("pack-bad-out").unwrap();
        let err = build(
            &config_with(vec![tool]),
            out.path(),
            &mut deps::Resolver::new(&Ldd),
            &Patchelf,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not usable as a name"), "{err}");
    }

    #[test]
    fn a_missing_data_path_is_named() {
        let work = TempDir::new("pack-missing").unwrap();
        let mut config = config_with(vec![]);
        config
            .data
            .push(data_entry(work.path().join("nope"), None, None));
        let out = TempDir::new("pack-missing-out").unwrap();
        let err = format!(
            "{:#}",
            build(
                &config,
                out.path(),
                &mut deps::Resolver::new(&Ldd),
                &Patchelf
            )
            .unwrap_err()
        );
        assert!(
            err.contains("data path") && err.contains("nope") && err.contains("does not exist"),
            "{err}"
        );
    }

    #[test]
    fn a_binary_for_another_architecture_is_refused() {
        let work = TempDir::new("pack-arch").unwrap();
        let mut tool = script_tool(work.path(), "foreign");
        // An ELF header for the architecture this machine is *not*.
        let machine: [u8; 2] = if Arch::host().ok() == Some(Arch::Aarch64) {
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
        tool.path = path;
        let out = TempDir::new("pack-arch-out").unwrap();
        let err = build(
            &config_with(vec![tool]),
            out.path(),
            &mut deps::Resolver::new(&Ldd),
            &Patchelf,
        )
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

    #[test]
    fn a_single_file_is_copied_to_the_destination_itself() {
        let dir = crate::tmpfile::TempDir::new("copy-tree").unwrap();
        let src = dir.path().join("rc");
        std::fs::write(&src, "set x").unwrap();
        let dst = dir.path().join("out/rc");
        copy_tree(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "set x");
    }
}
