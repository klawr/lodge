//! Injecting a binary: put the bundle's relinked copy in place and a wrapper where
//! the user expects it, and check that it fits the container.

use anyhow::{Result, bail};

use super::installed::{Decision, Kind, Piece, decide, record};
use crate::bundle::arch::Arch;
use crate::bundle::manifest::{Binary, Manifest, Tool};
use crate::bundle::source::Prepared;
use crate::layout;
use crate::output::info;
use crate::runtime::{ContainerUser, Env};

pub(super) fn inject_binary(
    env: &dyn Env,
    user: &ContainerUser,
    bundle: &Prepared,
    tool: &Tool,
    binary: &Binary,
) -> Result<()> {
    let name = &tool.name;
    let installed = format!("{}/{name}", layout::BIN);
    let dest = user.expand_home(&binary.dest);
    let piece = Piece {
        tool: name,
        kind: Kind::Binary,
    };

    match decide(
        env,
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
    env.cp_in(&bundle.bin(name).to_string_lossy(), &installed)?;
    env.exec_root(&["chmod", "+x", &installed])?;
    install_wrapper(env, &installed, &dest)?;
    record(env, piece, &binary.hash, &dest)
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

    env.write_file(dest, script.as_bytes())?;
    env.exec_root(&["chmod", "+x", dest])?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{FORMAT, Lib};
    use crate::bundle::source::Origin;
    use crate::runtime::fake::FakeEnv;
    use crate::tmpfile::TempDir;
    use std::collections::BTreeMap;

    fn tool(skip: bool, hash: &str) -> Tool {
        Tool {
            name: "hello".into(),
            binary: Some(Binary {
                dest: "/usr/local/bin/hello".into(),
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
            format: FORMAT,
            lodge_version: "0".into(),
            created: 0,
            arch: arch.into(),
            linker: None,
            libs: Vec::<Lib>::new(),
            tools: vec![tool],
            aliases: BTreeMap::new(),
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
        inject_binary(
            env,
            &ContainerUser::root(),
            bundle,
            tool,
            tool.binary.as_ref().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn wrapper_execs_the_injected_binary_and_is_executable() {
        let env = FakeEnv::new();
        install_wrapper(&env, "/opt/lodge/bin/nvim", "/usr/local/bin/nvim").unwrap();
        assert_eq!(
            env.file("/usr/local/bin/nvim").unwrap(),
            "#!/bin/sh\nexec /opt/lodge/bin/nvim \"$@\"\n"
        );
        assert!(env.ran(&["chmod", "+x", "/usr/local/bin/nvim"]));
    }

    #[test]
    fn a_binary_is_installed_with_its_wrapper_and_recorded() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa"), "x86_64");
        let env = FakeEnv::new();
        inject(&env, &b);
        assert_eq!(
            env.file("/opt/lodge/bin/hello").unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        assert!(
            env.file("/usr/local/bin/hello")
                .unwrap()
                .contains("exec /opt/lodge/bin/hello")
        );
        assert_eq!(
            env.file("/opt/lodge/installed/hello.bin").unwrap().trim(),
            "aaaaaaaaaaaaaaaa\n/usr/local/bin/hello"
        );
    }

    #[test]
    fn an_unchanged_binary_is_not_copied_again() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa"), "x86_64");
        let env = FakeEnv::new()
            .with_file("/opt/lodge/bin/hello", "old contents")
            .with_file("/opt/lodge/installed/hello.bin", "aaaaaaaaaaaaaaaa\n");
        inject(&env, &b);
        assert_eq!(env.file("/opt/lodge/bin/hello").unwrap(), "old contents");
    }

    #[test]
    fn a_changed_binary_is_replaced_unless_protected() {
        let installed = |env: FakeEnv| {
            env.with_file("/opt/lodge/bin/hello", "old contents")
                .with_file("/opt/lodge/installed/hello.bin", "1111111111111111\n")
        };
        let (_d1, replace) = bundle(tool(false, "aaaaaaaaaaaaaaaa"), "x86_64");
        let env = installed(FakeEnv::new());
        inject(&env, &replace);
        assert_eq!(
            env.file("/opt/lodge/bin/hello").unwrap(),
            "#!/bin/sh\necho hi\n"
        );

        let (_d2, protect) = bundle(tool(true, "aaaaaaaaaaaaaaaa"), "x86_64");
        let env = installed(FakeEnv::new());
        inject(&env, &protect);
        assert_eq!(env.file("/opt/lodge/bin/hello").unwrap(), "old contents");
    }

    #[test]
    fn a_bundle_for_another_architecture_is_refused() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa"), "x86_64");
        let env = FakeEnv::new().with_output(&["uname", "-m"], "aarch64\n");
        let err = check_architecture(&env, &b.manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("x86_64") && err.contains("aarch64"), "{err}");
    }

    #[test]
    fn matching_architecture_and_bundles_without_binaries_pass() {
        let (_dir, b) = bundle(tool(false, "aaaaaaaaaaaaaaaa"), "x86_64");
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
