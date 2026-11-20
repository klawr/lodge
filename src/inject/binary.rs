//! Injecting a binary: check it fits the container, relink it to the injected
//! glibc, and put a wrapper where the user expects it.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::config_files::resolve_home;
use super::libs::inject_deps;
use super::{LODGE_BIN, LODGE_DIR, LODGE_LIB};
use crate::config::Injectable;
use crate::output::info;
use crate::runtime::Env;
use crate::tmpfile::TempFile;
pub(super) fn inject_binary(
    env: &dyn Env,
    home: &str,
    item: &Injectable,
    binary: &str,
) -> Result<()> {
    let lodge_binary = format!("{LODGE_BIN}/{}", item.name);
    let target = resolve_home(
        item.target_binary
            .as_deref()
            .unwrap_or(&format!("/usr/local/bin/{}", item.name)),
        home,
    );

    if item.skip_if_exists && env.file_exists(&lodge_binary) {
        info!("{} already exists, skipping", item.name);
        return Ok(());
    }

    check_architecture(env, &item.name, binary)?;

    let linker = if item.copy_deps {
        inject_deps(env, binary, &item.name)?
    } else {
        None
    };

    info!("injecting {} binary", item.name);

    // With injected deps, patch a copy so the binary runs *directly* (no ld.so
    // wrapper) against our glibc — see patch_binary. Running directly keeps
    // /proc/self/exe pointing at the binary, which tools like Neovim need when
    // they re-exec themselves (`--embed`); baking the search path into the binary
    // (rather than exporting LD_LIBRARY_PATH) keeps subprocesses it spawns — git,
    // shells, LSPs — on the container's own glibc.
    match linker.as_deref() {
        Some(ld) => {
            let patched = patch_binary(binary, ld)?;
            env.cp_in(&patched.path().to_string_lossy(), &lodge_binary)?;
            // Make our linker available at /lodge/<ld> for the patched interpreter.
            env.exec_root(&[
                "cp",
                &format!("{LODGE_LIB}/{ld}"),
                &format!("{LODGE_DIR}/{ld}"),
            ])?;
        }
        None => {
            let real = std::fs::canonicalize(binary).unwrap_or_else(|_| PathBuf::from(binary));
            env.cp_in(&real.to_string_lossy(), &lodge_binary)?;
        }
    }

    env.exec_root(&["chmod", "+x", &lodge_binary])?;

    install_wrapper(env, &lodge_binary, &target)?;

    Ok(())
}

// Copies the binary to a self-cleaning temp file and uses `patchelf` to make it
// run *directly* against our injected glibc:
//   --set-interpreter /lodge/<ld> : our dynamic linker is the program interpreter
//   --force-rpath --set-rpath …   : DT_RPATH (not RUNPATH) so the search path
//                                   also resolves *transitive* deps (e.g. libnsl
//                                   pulling libtirpc); RUNPATH would not.
// patchelf handles the ELF rewriting robustly even when there's no slack in the
// original .dynstr/.dynamic — which is the common case for real binaries.
fn patch_binary(binary: &str, linker_name: &str) -> Result<TempFile> {
    let data = std::fs::read(binary).with_context(|| format!("reading {binary} to patch"))?;
    let tmp = TempFile::write(&format!("lodge-patched-{}", file_stem(binary)), &data)?;

    let mut cmd = Command::new("patchelf");
    cmd.arg("--set-interpreter")
        .arg(format!("{LODGE_DIR}/{linker_name}"))
        .arg("--force-rpath")
        .arg("--set-rpath")
        .arg(LODGE_LIB)
        .arg(tmp.path());
    crate::output::trace(&cmd);
    let status = cmd.status().context(
        "running patchelf — it is required to inject dynamically linked binaries \
             (install it, e.g. `pacman -S patchelf` / `apt install patchelf`)",
    )?;
    if !status.success() {
        bail!("patchelf failed to patch {binary}");
    }

    Ok(tmp)
}

// Refuses to inject a binary built for a different CPU than the container runs
// on: it would otherwise fail at startup with an unhelpful "exec format error"
// or "no such file". Scripts (not ELF) and unknown architectures are let through.
fn check_architecture(env: &dyn Env, name: &str, binary: &str) -> Result<()> {
    let Some(built_for) = crate::elf::arch_of(Path::new(binary))? else {
        return Ok(());
    };
    let Ok(uname) = env.exec_capture(&["uname", "-m"]) else {
        return Ok(());
    };
    let Some(runs_on) = crate::elf::Arch::from_uname(&uname) else {
        return Ok(());
    };
    if built_for != runs_on {
        bail!(
            "{name}: {binary} is built for {built_for}, but the container runs {runs_on}. \
             Inject a build for {runs_on} instead."
        );
    }
    Ok(())
}

// Installs a thin wrapper at `target` that execs the injected binary directly.
//
// The binary itself was patched (PT_INTERP + DT_RUNPATH) to run against our
// injected glibc, so the wrapper needs no LD_LIBRARY_PATH and no ld.so
// indirection — it just forwards args. Execing directly is what keeps
// /proc/self/exe pointing at the binary for tools that re-exec themselves.
fn install_wrapper(env: &dyn Env, lodge_binary: &str, target: &str) -> Result<()> {
    let script = format!("#!/bin/sh\nexec {lodge_binary} \"$@\"\n");

    env.write_file(target, script.as_bytes())?;
    env.exec_root(&["chmod", "+x", target])?;

    Ok(())
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "lodge".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn takes_basename() {
        assert_eq!(file_stem("/usr/local/bin/nvim"), "nvim");
        assert_eq!(file_stem("nvim"), "nvim");
    }

    #[test]
    fn falls_back_when_no_basename() {
        assert_eq!(file_stem("/"), "lodge");
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

    fn elf_file(machine: [u8; 2]) -> crate::tmpfile::TempFile {
        let mut h = vec![0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[5] = 1;
        h[18] = machine[0];
        h[19] = machine[1];
        crate::tmpfile::TempFile::write("arch-test", &h).unwrap()
    }

    #[test]
    fn injecting_for_the_wrong_architecture_is_refused() {
        let x86 = elf_file([0x3E, 0]);
        let env = FakeEnv::new().with_output(&["uname", "-m"], "aarch64\n");
        let err = check_architecture(&env, "nvim", &x86.path().to_string_lossy()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("x86_64") && msg.contains("aarch64"), "{msg}");
    }

    #[test]
    fn matching_architecture_and_non_elf_files_pass() {
        let x86 = elf_file([0x3E, 0]);
        let env = FakeEnv::new().with_output(&["uname", "-m"], "x86_64\n");
        check_architecture(&env, "nvim", &x86.path().to_string_lossy()).unwrap();

        let script =
            crate::tmpfile::TempFile::write("script-test", b"#!/bin/sh\necho hi\n").unwrap();
        let wrong = FakeEnv::new().with_output(&["uname", "-m"], "aarch64\n");
        check_architecture(&wrong, "tool", &script.path().to_string_lossy()).unwrap();
    }
}
