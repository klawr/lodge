//! Injecting a bundle into a target, for one user.
//!
//! Everything here is done *as that user* (through [`Acting`]) into a prefix they
//! own (see [`crate::layout`]), so it behaves the same whether the user is root, an
//! ordinary user, or rootless: it never asks for more than the user has.

mod binary;
mod config_files;
mod git;
mod installed;
mod libs;
mod prefix;
mod remove;
mod state;

use anyhow::Result;

use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::runtime::{Acting, ContainerUser, Env};
use binary::{check_architecture, inject_binary};
use config_files::inject_config;
use git::configure_git_safe;
use libs::inject_libs;
pub use prefix::{existing as existing_layout, resolve as resolve_layout};
pub use remove::remove;
pub use state::installed_bundle;
use state::{ensure_state_version, record_bundle};

/// Injects `bundle` for `user` into the prefix `layout` (see [`resolve_layout`]):
/// libraries first, then each tool's binary and config, then git's settings.
pub fn run(
    container: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    bundle: &Prepared,
) -> Result<()> {
    let env = Acting::new(container, user);
    check_architecture(&env, &bundle.manifest)?;
    ensure_state_version(&env, layout)?;

    inject_libs(&env, layout, bundle)?;
    for tool in &bundle.manifest.tools {
        if let Some(binary) = &tool.binary {
            inject_binary(&env, user, layout, bundle, tool, binary)?;
        }
        if let Some(config) = &tool.config {
            inject_config(&env, user, layout, bundle, tool, config)?;
        }
    }
    record_bundle(&env, layout, &bundle.manifest.id())?;

    configure_git_safe(&env, &bundle.manifest.git_safe_directories)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{Binary, ConfigTree, FORMAT, Manifest, Tool};
    use crate::bundle::source::Origin;
    use crate::runtime::fake::{FakeEnv, Who};
    use crate::tmpfile::TempDir;

    fn dev() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    #[test]
    fn a_bundle_is_injected_whole_for_a_user_with_no_root_and_recorded() {
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
                    dest: None,
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
            .with_commands(&["git"]);
        let layout = Layout::preferred(&dev());

        run(&env, &dev(), &layout, &bundle).unwrap();

        let p = layout.prefix();
        assert!(env.file(&format!("{p}/bin/hello")).is_some());
        assert_eq!(
            env.file("/home/dev/.hellorc").as_deref(),
            Some("greeting=hi")
        );
        assert_eq!(env.file(&format!("{p}/bundle")), Some(format!("{id}\n")));
        assert_eq!(env.file(&format!("{p}/version")).as_deref(), Some("2\n"));
        for (call, who) in env.calls() {
            assert_ne!(who, Who::Root, "{call:?} ran as root");
        }
        // Nothing outside the prefix and the user's own home was written.
        assert!(env.file("/usr/local/bin/hello").is_none());
        assert!(env.file("/etc/profile.d/lodge.sh").is_none());
        assert!(env.file("/opt/lodge/bin/hello").is_none());
    }

    #[test]
    fn it_works_the_same_for_root() {
        let dir = TempDir::new("inject-root").unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/c"), "x").unwrap();
        let manifest = Manifest {
            format: FORMAT,
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: None,
            libs: vec![],
            tools: vec![Tool {
                name: "c".into(),
                binary: None,
                config: Some(ConfigTree {
                    dest: "~/.c".into(),
                    is_dir: false,
                    hash: "00000000000000bb".into(),
                }),
                skip_if_exists: false,
            }],
            git_safe_directories: vec![],
        };
        let bundle = Prepared {
            dir: dir.path().to_path_buf(),
            manifest,
            origin: Origin::Packed,
        };
        let env = FakeEnv::new();
        let root = ContainerUser::root();
        run(&env, &root, &Layout::preferred(&root), &bundle).unwrap();
        assert_eq!(env.file("/root/.c").as_deref(), Some("x"));
        assert!(env.file("/root/.local/share/lodge/bundle").is_some());
    }
}
