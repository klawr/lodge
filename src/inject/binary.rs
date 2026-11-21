use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::arch::Arch;
use crate::bundle::elf;
use crate::bundle::manifest::{BIN_DIR, Binary, Manifest, Tool};
use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::runtime::{ContainerUser, Env, ExecOpts};

/// Puts the tool's binary into `stage/bin/`, which is sent as a whole to the
/// prefix.
///
/// The bundle's copy was relinked when it was packed, so it runs *directly*
/// against the injected glibc (no ld.so wrapper). Running directly keeps
/// /proc/self/exe pointing at the binary, which tools like Neovim need when
/// they re-exec themselves (`--embed`). A dynamic one is staged as a copy whose
/// interpreter names the dynamic linker in `layout`; a static one as it is.
pub(super) fn stage_binary(
    bundle: &Prepared,
    layout: &Layout,
    tool: &Tool,
    binary: &Binary,
    stage: &Path,
) -> Result<()> {
    let name = &tool.name;
    let source = bundle.bin(name);
    let dir = stage.join(BIN_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    let to = dir.join(name);
    if !binary.dynamic {
        // A link costs nothing; a copy is for a bundle cache on another filesystem.
        if std::fs::hard_link(&source, &to).is_err() {
            std::fs::copy(&source, &to).with_context(|| format!("staging {}", source.display()))?;
        }
        return Ok(());
    }
    let linker = bundle
        .manifest
        .linker
        .as_deref()
        .with_context(|| format!("{name} is dynamic but the bundle names no linker"))?;
    let mut bytes =
        std::fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
    elf::set_interpreter(&mut bytes, &layout.lib_file(linker))
        .with_context(|| format!("preparing {name} to run from {}", layout.prefix()))?;
    std::fs::write(&to, &bytes).with_context(|| format!("staging {}", to.display()))?;
    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Where the config asked for an extra wrapper (a script, so any program that
/// looks for the tool at that path finds it), writes it and returns its path.
pub(super) fn place_wrapper(
    env: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    tool: &Tool,
    binary: &Binary,
) -> Result<Option<String>> {
    let Some(dest) = binary.dest.as_deref().map(|d| user.expand_home(d)) else {
        return Ok(None);
    };
    install_wrapper(env, &layout.bin_of(&tool.name), &dest)
        .with_context(|| format!("{}: writing the wrapper {dest}", tool.name))?;
    Ok(Some(dest))
}

// Refuses a bundle built for a different CPU than the container runs on: its
// binaries would otherwise fail at startup with an unhelpful "exec format error"
// or "no such file". A bundle with no binaries (just configs) fits anywhere, and
// an architecture we can't identify is let through.
pub(super) fn check_architecture(env: &dyn Env, manifest: &Manifest) -> Result<()> {
    if !manifest.tools.iter().any(|t| t.binary.is_some()) {
        return Ok(());
    }
    // A container is Linux whatever runs the engine; a real machine may not be,
    // and a bundle holds Linux binaries.
    if !env.is_disposable()
        && let Ok(os) = env.exec_capture_with(&["uname", "-s"], &ExecOpts::default().repeatable())
        && os.trim() != "Linux"
    {
        bail!(
            "the bundle's binaries are for Linux, but this machine runs {}",
            os.trim()
        );
    }
    let Ok(bundle_arch) = manifest.arch() else {
        return Ok(());
    };
    let Ok(uname) = env.exec_capture_with(&["uname", "-m"], &ExecOpts::default().repeatable())
    else {
        return Ok(());
    };
    let Some(runs_on) = Arch::from_uname(&uname) else {
        return Ok(());
    };
    if bundle_arch != runs_on {
        bail!(
            "the bundle is for {bundle_arch}, but the container runs {runs_on}. \
             Use a {runs_on} bundle (pack one on a {runs_on} machine)."
        );
    }
    Ok(())
}

// Installs a thin wrapper at `dest` that execs the injected binary directly.
//
// The binary itself was relinked (interpreter + search path) to run against our
// injected glibc, so the wrapper needs no LD_LIBRARY_PATH and no ld.so
// indirection — it just forwards args. Execing directly is what keeps
// /proc/self/exe pointing at the binary for tools that re-exec themselves.
fn install_wrapper(env: &dyn Env, lodge_binary: &str, dest: &str) -> Result<()> {
    let script = format!("#!/bin/sh\nexec {lodge_binary} \"$@\"\n");
    env.write_file_mode(dest, script.as_bytes(), 0o755)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::Lib;
    use crate::bundle::source::Origin;
    use crate::runtime::Acting;
    use crate::runtime::fake::FakeEnv;
    use crate::tmpfile::TempDir;

    fn layout() -> Layout {
        Layout::new("/p")
    }

    fn dev() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    fn tool(wrapper: Option<&str>) -> Tool {
        Tool {
            name: "hello".into(),
            binary: Some(Binary {
                dest: wrapper.map(String::from),
                dynamic: false,
                hash: "aaaaaaaaaaaaaaaa".into(),
            }),
            config: None,
        }
    }

    /// A bundle directory with one binary `hello`.
    fn bundle(tool: Tool, arch: &str) -> (TempDir, Prepared) {
        let dir = TempDir::new("bin-bundle").unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
        let manifest = Manifest {
            lodge_version: "0".into(),
            created: 0,
            arch: arch.into(),
            linker: None,
            libs: Vec::<Lib>::new(),
            tools: vec![tool],
            git_safe_directories: vec![],
        };
        let prepared = Prepared {
            dir: dir.path().to_path_buf(),
            manifest,
            origin: Origin::Packed,
        };
        (dir, prepared)
    }

    /// Stages the bundle's first tool for `layout` and returns what was staged.
    fn staged(bundle: &Prepared, layout: &Layout) -> Result<(TempDir, std::path::PathBuf)> {
        let stage = TempDir::new("bin-stage").unwrap();
        let tool = &bundle.manifest.tools[0];
        stage_binary(
            bundle,
            layout,
            tool,
            tool.binary.as_ref().unwrap(),
            stage.path(),
        )?;
        let file = stage.path().join("bin/hello");
        Ok((stage, file))
    }

    #[test]
    fn wrapper_execs_the_injected_binary_and_is_executable() {
        let env = FakeEnv::new();
        install_wrapper(&env, "/p/bin/nvim", "/home/dev/.local/bin/nvim").unwrap();
        assert_eq!(
            env.file("/home/dev/.local/bin/nvim").unwrap(),
            "#!/bin/sh\nexec /p/bin/nvim \"$@\"\n"
        );
        assert_eq!(env.mode("/home/dev/.local/bin/nvim"), Some(0o755));
    }

    #[test]
    fn a_static_binary_is_staged_as_it_is_in_bin() {
        let (_dir, b) = bundle(tool(None), "x86_64");
        let (_stage, file) = staged(&b, &layout()).unwrap();
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
    }

    #[test]
    fn a_wrapper_is_written_only_where_the_config_asked_for_one() {
        let (_dir, none) = bundle(tool(None), "x86_64");
        let env = FakeEnv::new();
        let user = dev();
        let place = |b: &Prepared| {
            let tool = &b.manifest.tools[0];
            place_wrapper(
                &Acting::new(&env, &user),
                &user,
                &layout(),
                tool,
                tool.binary.as_ref().unwrap(),
            )
            .unwrap()
        };
        assert_eq!(place(&none), None);
        assert!(!env.ran_matching(|_| true), "nothing was written");

        let (_dir, some) = bundle(tool(Some("~/.local/bin/hello")), "x86_64");
        assert_eq!(
            place(&some).as_deref(),
            Some("/home/dev/.local/bin/hello"),
            "the path is returned so it is recorded and `ldg clean` can remove it"
        );
        assert!(
            env.file("/home/dev/.local/bin/hello")
                .unwrap()
                .contains("exec /p/bin/hello")
        );
    }

    /// An ELF64 image with a placeholder interpreter, as `ldg pack` leaves it.
    fn dynamic_image() -> Vec<u8> {
        let cap = elf::INTERP_CAPACITY;
        let mut v = vec![0u8; 128 + cap];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2;
        v[5] = 1;
        v[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        v[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        v[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes());
        v[64..68].copy_from_slice(&3u32.to_le_bytes());
        v[72..80].copy_from_slice(&128u64.to_le_bytes());
        v[96..104].copy_from_slice(&(cap as u64).to_le_bytes());
        let placeholder = elf::placeholder_interpreter();
        v[128..128 + placeholder.len()].copy_from_slice(placeholder.as_bytes());
        v
    }

    #[test]
    fn a_dynamic_binary_is_given_the_interpreter_of_the_prefix_it_goes_to() {
        let (dir, mut b) = bundle(tool(None), "x86_64");
        std::fs::write(dir.path().join("bin/hello"), dynamic_image()).unwrap();
        b.manifest.linker = Some("ld-linux-x86-64.so.2".into());
        b.manifest.tools[0].binary.as_mut().unwrap().dynamic = true;

        for prefix in ["/p", "/home/dev/.local/share/lodge"] {
            let (_stage, file) = staged(&b, &Layout::new(prefix)).unwrap();
            let image = std::fs::read(&file).unwrap();
            let interp = elf::interpreter(&image).unwrap().unwrap();
            assert_eq!(interp, format!("{prefix}/lib/ld-linux-x86-64.so.2"));
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        // The bundle's own copy still holds the placeholder.
        let kept = std::fs::read(dir.path().join("bin/hello")).unwrap();
        assert_eq!(
            elf::interpreter(&kept).unwrap(),
            Some(elf::placeholder_interpreter())
        );
    }

    #[test]
    fn a_prefix_too_long_for_the_reserved_space_is_refused_by_name() {
        let (dir, mut b) = bundle(tool(None), "x86_64");
        std::fs::write(dir.path().join("bin/hello"), dynamic_image()).unwrap();
        b.manifest.linker = Some("ld.so".into());
        b.manifest.tools[0].binary.as_mut().unwrap().dynamic = true;
        let long = Layout::new(format!("/{}", "a".repeat(600)));
        let err = format!("{:#}", staged(&b, &long).err().unwrap());
        assert!(err.contains("hello") && err.contains("shorter"), "{err}");
    }

    #[test]
    fn a_bundle_for_another_architecture_is_refused() {
        let (_dir, b) = bundle(tool(None), "x86_64");
        let env = FakeEnv::new().with_output(&["uname", "-m"], "aarch64\n");
        let err = check_architecture(&env, &b.manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("x86_64") && err.contains("aarch64"), "{err}");
    }

    #[test]
    fn matching_architecture_and_bundles_without_binaries_pass() {
        let (_dir, b) = bundle(tool(None), "x86_64");
        let env = FakeEnv::new().with_output(&["uname", "-m"], "x86_64\n");
        check_architecture(&env, &b.manifest).unwrap();

        let mut configs_only = b.manifest.clone();
        configs_only.tools[0].binary = None;
        configs_only.tools[0].config = Some(crate::bundle::manifest::ConfigTree {
            dest: "~/.x".into(),
            is_dir: false,
            hash: "0000000000000000".into(),
        });
        // No `uname` is declared: a bundle with no binaries must not even ask.
        check_architecture(&FakeEnv::new(), &configs_only).unwrap();
    }

    #[test]
    fn a_real_machine_that_is_not_linux_is_refused_but_a_container_is_never_asked() {
        let manifest = Manifest {
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: None,
            libs: vec![],
            tools: vec![crate::bundle::manifest::Tool {
                name: "t".into(),
                binary: Some(crate::bundle::manifest::Binary {
                    dest: None,
                    dynamic: false,
                    hash: "00000000000000aa".into(),
                }),
                config: None,
            }],
            git_safe_directories: vec![],
        };
        let mac = FakeEnv::new()
            .shared()
            .with_output(&["uname", "-s"], "Darwin\n");
        let err = check_architecture(&mac, &manifest).unwrap_err();
        assert!(err.to_string().contains("Darwin"), "{err}");
        let linux = FakeEnv::new()
            .shared()
            .with_output(&["uname", "-s"], "Linux\n")
            .with_output(&["uname", "-m"], "x86_64\n");
        check_architecture(&linux, &manifest).unwrap();
        // A container declares no `uname -s`: asking would panic the strict fake.
        let container = FakeEnv::new().with_output(&["uname", "-m"], "x86_64\n");
        check_architecture(&container, &manifest).unwrap();
    }
}
