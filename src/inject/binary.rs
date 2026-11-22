use anyhow::{Context, Result, bail};

use crate::arch::Arch;
use crate::bundle::manifest::{Binary, Manifest};
use crate::layout::Layout;
use crate::runtime::{ContainerUser, Env, ExecOpts};

/// Where the config asked for an extra wrapper (a script, so any program that
/// looks for the tool at that path finds it), writes it and returns its path.
pub(super) fn place_wrapper(
    env: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    binary: &Binary,
) -> Result<Option<String>> {
    let Some(dest) = binary.dest.as_deref().map(|d| user.expand_home(d)) else {
        return Ok(None);
    };
    install_wrapper(env, &layout.bin_of(&binary.name), &dest)
        .with_context(|| format!("{}: writing the wrapper {dest}", binary.name))?;
    Ok(Some(dest))
}

// Refuses a bundle built for a different CPU than the container runs on: its
// binaries would otherwise fail at startup with an unhelpful "exec format error"
// or "no such file". A bundle with no binaries (just data) fits anywhere, and
// an architecture we can't identify is let through.
pub(super) fn check_architecture(env: &dyn Env, manifest: &Manifest) -> Result<()> {
    if manifest.binaries.is_empty() {
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
    use crate::bundle::testing::manifest;
    use crate::runtime::Acting;
    use crate::runtime::fake::FakeEnv;

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

    fn hello(wrapper: Option<&str>) -> Binary {
        Binary {
            name: "hello".into(),
            dest: wrapper.map(String::from),
            dynamic: false,
            hash: "aaaaaaaaaaaaaaaa".into(),
        }
    }

    /// A manifest for `arch` listing the one binary `hello`.
    fn manifest_of(binary: Binary, arch: &str) -> Manifest {
        Manifest {
            binaries: vec![binary],
            ..manifest(arch)
        }
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
    fn a_wrapper_is_written_only_where_the_config_asked_for_one() {
        let none = manifest_of(hello(None), "x86_64");
        let env = FakeEnv::new();
        let user = dev();
        let place = |m: &Manifest| {
            place_wrapper(&Acting::new(&env, &user), &user, &layout(), &m.binaries[0]).unwrap()
        };
        assert_eq!(place(&none), None);
        assert!(!env.ran_matching(|_| true), "nothing was written");

        let some = manifest_of(hello(Some("~/.local/bin/hello")), "x86_64");
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

    #[test]
    fn a_bundle_for_another_architecture_is_refused() {
        let m = manifest_of(hello(None), "x86_64");
        let env = FakeEnv::new().with_output(&["uname", "-m"], "aarch64\n");
        let err = check_architecture(&env, &m).unwrap_err().to_string();
        assert!(err.contains("x86_64") && err.contains("aarch64"), "{err}");
    }

    #[test]
    fn matching_architecture_and_bundles_without_binaries_pass() {
        let m = manifest_of(hello(None), "x86_64");
        let env = FakeEnv::new().with_output(&["uname", "-m"], "x86_64\n");
        check_architecture(&env, &m).unwrap();

        let mut data_only = m.clone();
        data_only.binaries.clear();
        data_only.data.push(crate::bundle::manifest::Data {
            dest: "~/.x".into(),
            is_dir: false,
            hash: "0000000000000000".into(),
            binary: None,
        });
        // No `uname` is declared: a bundle with no binaries must not even ask.
        check_architecture(&FakeEnv::new(), &data_only).unwrap();
    }

    #[test]
    fn a_real_machine_that_is_not_linux_is_refused_but_a_container_is_never_asked() {
        let manifest = manifest_of(
            Binary {
                name: "t".into(),
                dest: None,
                dynamic: false,
                hash: "00000000000000aa".into(),
            },
            "x86_64",
        );
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
