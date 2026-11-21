use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::installed::{Decision, Kind, Piece, decide, record};
use crate::bundle::arch::Arch;
use crate::bundle::elf;
use crate::bundle::manifest::{Binary, Manifest, Tool};
use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::output::info;
use crate::runtime::{ContainerUser, Env};
use crate::tmpfile::TempFile;

pub(super) fn inject_binary(
    env: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    bundle: &Prepared,
    tool: &Tool,
    binary: &Binary,
) -> Result<()> {
    let name = &tool.name;
    let installed = layout.bin_of(name);
    let dir = layout.installed();
    let piece = Piece {
        tool: name,
        kind: Kind::Binary,
    };

    match decide(
        env,
        &dir,
        piece,
        &binary.hash,
        env.exists(&installed)?,
        tool.skip_if_exists,
    ) {
        Decision::UpToDate => {
            info!("{name} is up to date");
            return Ok(());
        }
        Decision::KeepExisting => {
            info!("{name} already exists, skipping");
            return Ok(());
        }
        Decision::Install => {}
    }

    info!("injecting {name} binary");
    // The bundle's copy was relinked when it was packed, so it runs *directly*
    // against the injected glibc (no ld.so wrapper). Running directly keeps
    // /proc/self/exe pointing at the binary, which tools like Neovim need when
    // they re-exec themselves (`--embed`).
    let staged = stage(bundle, layout, name, binary)?;
    env.cp_in(&staged.path().to_string_lossy(), &installed)?;
    // Where the user asked for an extra wrapper: a script, so any program that
    // looks for the tool at that path finds it.
    let wrapper = binary.dest.as_deref().map(|d| user.expand_home(d));
    if let Some(dest) = &wrapper {
        install_wrapper(env, &installed, dest)
            .with_context(|| format!("{name}: writing the wrapper {dest}"))?;
    }
    record(
        env,
        &dir,
        piece,
        &binary.hash,
        wrapper.as_deref().unwrap_or(&installed),
    )
}

/// A binary ready to copy: the bundle's own file, or (for a dynamic one) a copy
/// of it whose interpreter now names the dynamic linker in `layout`.
enum Staged {
    Bundle(PathBuf),
    Rewritten(TempFile),
}

impl Staged {
    fn path(&self) -> &std::path::Path {
        match self {
            Staged::Bundle(p) => p,
            Staged::Rewritten(t) => t.path(),
        }
    }
}

fn stage(bundle: &Prepared, layout: &Layout, name: &str, binary: &Binary) -> Result<Staged> {
    let source = bundle.bin(name);
    if !binary.dynamic {
        return Ok(Staged::Bundle(source));
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
    let tmp = TempFile::write(&format!("lodge-{name}"), &bytes)?;
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))?;
    Ok(Staged::Rewritten(tmp))
}

// Refuses a bundle built for a different CPU than the container runs on: its
// binaries would otherwise fail at startup with an unhelpful "exec format error"
// or "no such file". A bundle with no binaries (just configs) fits anywhere, and
// an architecture we can't identify is let through.
pub(super) fn check_architecture(env: &dyn Env, manifest: &Manifest) -> Result<()> {
    if !manifest.tools.iter().any(|t| t.binary.is_some()) {
        return Ok(());
    }
    let Ok(bundle_arch) = manifest.arch() else {
        return Ok(());
    };
    let Ok(uname) = env.exec_capture(&["uname", "-m"]) else {
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

    fn tool(skip: bool, hash: &str, wrapper: Option<&str>) -> Tool {
        Tool {
            name: "hello".into(),
            binary: Some(Binary {
                dest: wrapper.map(String::from),
                dynamic: false,
                hash: hash.into(),
            }),
            config: None,
            skip_if_exists: skip,
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

    fn inject(env: &FakeEnv, bundle: &Prepared) {
        let tool = &bundle.manifest.tools[0];
        let user = dev();
        inject_binary(
            &Acting::new(env, &user),
            &user,
            &layout(),
            bundle,
            tool,
            tool.binary.as_ref().unwrap(),
        )
        .unwrap();
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
    fn a_binary_is_installed_into_the_prefix_and_recorded_with_no_wrapper() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
        let env = FakeEnv::new();
        inject(&env, &b);
        assert_eq!(env.file("/p/bin/hello").unwrap(), "#!/bin/sh\necho hi\n");
        assert_eq!(
            env.file("/p/installed/hello.bin").unwrap().trim(),
            "aaaaaaaaaaaaaaaa\n/p/bin/hello"
        );
        assert!(
            !env.ran_matching(|c| c[0] == "chmod" || c[0] == "chown"),
            "mode and owner come with the copy"
        );
        assert_eq!(
            env.owner_of("/p/bin/hello"),
            Some((1000, 1000)),
            "the copy is made for the user, who then owns it"
        );
    }

    #[test]
    fn a_wrapper_is_written_only_where_the_config_asked_for_one() {
        let (_dir, b) = bundle(
            tool(false, "aaaaaaaaaaaaaaaa", Some("~/.local/bin/hello")),
            "x86_64",
        );
        let env = FakeEnv::new();
        inject(&env, &b);
        assert!(
            env.file("/home/dev/.local/bin/hello")
                .unwrap()
                .contains("exec /p/bin/hello")
        );
        assert_eq!(
            env.file("/p/installed/hello.bin").unwrap().trim(),
            "aaaaaaaaaaaaaaaa\n/home/dev/.local/bin/hello",
            "the record names the wrapper, so `ldg clean` can remove it"
        );
    }

    #[test]
    fn an_unchanged_binary_is_not_copied_again() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
        let env = FakeEnv::new()
            .with_file("/p/bin/hello", "old contents")
            .with_file("/p/installed/hello.bin", "aaaaaaaaaaaaaaaa\n/p/bin/hello");
        inject(&env, &b);
        assert_eq!(env.file("/p/bin/hello").unwrap(), "old contents");
    }

    #[test]
    fn a_changed_binary_is_replaced_unless_protected() {
        let installed = |env: FakeEnv| {
            env.with_file("/p/bin/hello", "old contents")
                .with_file("/p/installed/hello.bin", "1111111111111111\n")
        };
        let (_d1, replace) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
        let env = installed(FakeEnv::new());
        inject(&env, &replace);
        assert_eq!(env.file("/p/bin/hello").unwrap(), "#!/bin/sh\necho hi\n");

        let (_d2, protect) = bundle(tool(true, "aaaaaaaaaaaaaaaa", None), "x86_64");
        let env = installed(FakeEnv::new());
        inject(&env, &protect);
        assert_eq!(env.file("/p/bin/hello").unwrap(), "old contents");
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
        let (dir, mut b) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
        std::fs::write(dir.path().join("bin/hello"), dynamic_image()).unwrap();
        b.manifest.linker = Some("ld-linux-x86-64.so.2".into());
        b.manifest.tools[0].binary.as_mut().unwrap().dynamic = true;

        for prefix in ["/p", "/home/dev/.local/share/lodge"] {
            let env = FakeEnv::new();
            let tool = &b.manifest.tools[0];
            inject_binary(
                &env,
                &dev(),
                &Layout::new(prefix),
                &b,
                tool,
                tool.binary.as_ref().unwrap(),
            )
            .unwrap();
            let installed = env.file_bytes(&format!("{prefix}/bin/hello")).unwrap();
            let interp = elf::interpreter(&installed).unwrap().unwrap();
            assert_eq!(interp, format!("{prefix}/lib/ld-linux-x86-64.so.2"));
            assert_eq!(env.mode(&format!("{prefix}/bin/hello")), Some(0o755));
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
        let (dir, mut b) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
        std::fs::write(dir.path().join("bin/hello"), dynamic_image()).unwrap();
        b.manifest.linker = Some("ld.so".into());
        b.manifest.tools[0].binary.as_mut().unwrap().dynamic = true;
        let long = Layout::new(format!("/{}", "a".repeat(600)));
        let env = FakeEnv::new();
        let tool = &b.manifest.tools[0];
        let err = format!(
            "{:#}",
            inject_binary(&env, &dev(), &long, &b, tool, tool.binary.as_ref().unwrap())
                .unwrap_err()
        );
        assert!(err.contains("hello") && err.contains("shorter"), "{err}");
        assert!(env.file(&format!("{}/bin/hello", long.prefix())).is_none());
    }

    #[test]
    fn a_bundle_for_another_architecture_is_refused() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
        let env = FakeEnv::new().with_output(&["uname", "-m"], "aarch64\n");
        let err = check_architecture(&env, &b.manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("x86_64") && err.contains("aarch64"), "{err}");
    }

    #[test]
    fn matching_architecture_and_bundles_without_binaries_pass() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa", None), "x86_64");
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
}
