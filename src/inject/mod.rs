//! Putting the user's environment into a container.
//!
//! [`run`] is the entry point. Everything is read from a bundle directory
//! ([`crate::bundle::source`]), never from the host's own files, so injecting
//! needs neither `ldd` nor `patchelf`. The work is split by what is being
//! injected:
//!
//! - [`binary`]: a binary, already relinked, behind a wrapper
//! - [`libs`]: its shared libraries and the dynamic linker, tracked by content
//! - [`installed`]: a hash per installed piece, so unchanged means a cheap no-op
//! - [`config_files`]: config files and directories
//! - [`home`]: the user's home directory: writable, and owning what is put there
//! - [`shell_env`]: `PATH`, aliases, git's `safe.directory`
//! - [`state`]: the format version of what is kept under `/opt/lodge`, and the
//!   id of the bundle injected
//! - [`mod@remove`]: taking it all back out (`ldg clean`)

mod binary;
mod config_files;
mod home;
mod installed;
mod libs;
mod remove;
mod shell_env;
mod state;

use anyhow::Result;

use crate::bundle::source::Prepared;
use crate::layout;
use crate::runtime::{ContainerUser, Env};
use binary::{check_architecture, inject_binary};
use config_files::inject_config;
use home::ensure_home_writable;
use libs::inject_libs;
pub use remove::remove;
use shell_env::{configure_git_safe, inject_aliases, preserve_path};
pub use state::installed_bundle;
use state::{ensure_state_version, record_bundle};

/// Injects `bundle` into the environment for `user`: libraries first, then each
/// tool's binary and config, then the shell-level setup.
pub fn run(env: &dyn Env, user: &ContainerUser, bundle: &Prepared) -> Result<()> {
    check_architecture(env, &bundle.manifest)?;
    env.mkdir_p(&[layout::LIB, layout::BIN, layout::LINKER_DIR])?;
    ensure_state_version(env)?;
    ensure_home_writable(env, user)?;

    inject_libs(env, bundle)?;
    for tool in &bundle.manifest.tools {
        if let Some(binary) = &tool.binary {
            inject_binary(env, user, bundle, tool, binary)?;
        }
        if let Some(config) = &tool.config {
            inject_config(env, user, bundle, tool, config)?;
        }
    }
    record_bundle(env, &bundle.manifest.id())?;

    preserve_path(env)?;
    configure_git_safe(env, &bundle.manifest.git_safe_directories, user.exec_as())?;
    inject_aliases(env, &bundle.manifest.aliases)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{Binary, ConfigTree, FORMAT, Manifest, Tool};
    use crate::bundle::source::Origin;
    use crate::runtime::fake::FakeEnv;
    use crate::tmpfile::TempDir;
    use std::collections::BTreeMap;

    #[test]
    fn a_bundle_is_injected_whole_and_recorded() {
        let dir = TempDir::new("inject-run").unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(dir.path().join("config/hello"), "greeting=hi").unwrap();
        let manifest = Manifest {
            format: FORMAT,
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: None,
            libs: vec![],
            tools: vec![Tool {
                name: "hello".into(),
                binary: Some(Binary {
                    dest: "/usr/local/bin/hello".into(),
                    dynamic: false,
                    hash: "00000000000000aa".into(),
                }),
                config: Some(ConfigTree {
                    dest: "~/.hellorc".into(),
                    is_dir: false,
                    hash: "00000000000000bb".into(),
                }),
                skip_if_exists: false,
            }],
            aliases: BTreeMap::from([("hi".into(), "hello".into())]),
            git_safe_directories: vec!["*".into()],
        };
        let id = manifest.id();
        let bundle = Prepared {
            dir: dir.path().to_path_buf(),
            manifest,
            origin: Origin::Packed,
        };
        let env = FakeEnv::new()
            .with_output(&["uname", "-m"], "x86_64\n")
            .with_var("PATH", "/usr/bin")
            .with_commands(&["git"]);

        run(&env, &ContainerUser::root(), &bundle).unwrap();

        assert!(env.file("/opt/lodge/bin/hello").is_some());
        assert!(
            env.file("/usr/local/bin/hello")
                .unwrap()
                .contains("exec /opt/lodge/bin/hello")
        );
        assert_eq!(env.file("/root/.hellorc").as_deref(), Some("greeting=hi"));
        assert_eq!(env.file("/opt/lodge/bundle"), Some(format!("{id}\n")));
        assert_eq!(env.file("/opt/lodge/version").as_deref(), Some("1\n"));
        assert!(
            env.file("/etc/profile.d/lodge.sh")
                .unwrap()
                .contains("alias hi='hello'")
        );
        assert!(env.file("/etc/profile.d/lodge-path.sh").is_some());
    }
}
