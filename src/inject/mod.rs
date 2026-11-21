//! Injecting a bundle into a target, for one user.
//!
//! Everything here is done *as that user* (through [`Acting`]) into a prefix they
//! own (see [`crate::layout`]), so it behaves the same whether the user is root, an
//! ordinary user, or rootless: it never asks for more than the user has.
//!
//! An injection always replaces what is there: the libraries and binaries are
//! staged in a local directory, sent to the prefix as one tar, and the configs are
//! copied over their destinations. Whether to inject at all is the caller's
//! decision (see `ldg up`), not something this module second-guesses.

mod binary;
mod config_files;
mod git;
mod libs;
mod prefix;
mod remove;
mod state;

use anyhow::{Context, Result};

use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::output::{info, plural};
use crate::runtime::{Acting, ContainerUser, Env, ExecOpts};
use crate::tmpfile::TempDir;
use binary::{check_architecture, place_wrapper, stage_binary};
use config_files::inject_config;
use git::configure_git_safe;
use libs::stage_libs;
pub use prefix::resolve as resolve_layout;
pub use remove::remove;
pub use state::{Installed, installed};

/// Injects `bundle` for `user` into the prefix `layout` (see [`resolve_layout`]):
/// libraries and binaries in one copy, then each tool's wrapper and config, then
/// git's settings. The record that says what was injected is written last, so an
/// injection that failed halfway reads as none.
pub fn run(
    container: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    bundle: &Prepared,
) -> Result<()> {
    let env = Acting::new(container, user);
    check_architecture(&env, &bundle.manifest)?;

    let stage = TempDir::new("inject").context("making a staging directory")?;
    stage_libs(bundle, stage.path())?;
    for tool in &bundle.manifest.tools {
        if let Some(binary) = &tool.binary {
            stage_binary(bundle, layout, tool, binary, stage.path())?;
        }
    }
    let binaries = bundle
        .manifest
        .tools
        .iter()
        .filter(|t| t.binary.is_some())
        .count();
    let libs = bundle.manifest.libs.len();
    let contents: Vec<String> = [
        (binaries, "binary", "binaries"),
        (libs, "library", "libraries"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, one, many)| plural(n, one, many))
    .collect();
    if contents.is_empty() {
        info!("injecting bundle {}", bundle.manifest.id());
    } else {
        info!(
            "injecting bundle {} ({})",
            bundle.manifest.id(),
            contents.join(", ")
        );
    }
    // What an earlier injection placed outside the prefix, to tell it from what
    // was already there.
    let previous = state::previous(&env, layout)?;
    let placed_before = previous
        .as_ref()
        .map(|p| p.configs.clone())
        .unwrap_or_default();
    let safe_before = previous.map(|p| p.safe_directories).unwrap_or_default();
    // `bin/` and `lib/` are replaced as a whole, so nothing of an earlier
    // injection is left behind. (A running binary can be unlinked, but not
    // overwritten.)
    env.exec(
        &["rm", "-rf", &layout.bin(), &layout.lib()],
        &ExecOpts::default().repeatable(),
    )?;
    env.cp_in(&stage.path().to_string_lossy(), layout.prefix())?;

    let mut wrappers = Vec::new();
    let mut configs = Vec::new();
    for tool in &bundle.manifest.tools {
        if let Some(binary) = &tool.binary
            && let Some(wrapper) = place_wrapper(&env, user, layout, tool, binary)?
        {
            wrappers.push(wrapper);
        }
        if let Some(config) = &tool.config {
            configs.push(inject_config(
                &env,
                user,
                bundle,
                tool,
                config,
                &placed_before,
            )?);
        }
    }
    let safe_directories =
        configure_git_safe(&env, &bundle.manifest.git_safe_directories, &safe_before)?;

    state::record(
        &env,
        &Installed {
            layout: layout.clone(),
            bundle: bundle.manifest.id(),
            wrappers,
            configs,
            safe_directories,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{Binary, ConfigTree, Manifest, Tool};
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
            .with_commands(&["git"])
            .with_output(
                &[
                    "sh",
                    "-c",
                    "git config --global --get-all safe.directory 2>/dev/null | grep -qxF '*' \
                     || { git config --global --add safe.directory '*' && echo added; }",
                ],
                "added\n",
            );
        let layout = Layout::preferred(&dev());

        run(&env, &dev(), &layout, &bundle).unwrap();

        let p = layout.prefix();
        assert!(env.file(&format!("{p}/bin/hello")).is_some());
        assert_eq!(
            env.file("/home/dev/.hellorc").as_deref(),
            Some("greeting=hi")
        );
        assert_eq!(
            env.file(&format!("{p}/injected")),
            Some(format!(
                "bundle {id}\nconfig /home/dev/.hellorc\nsafe_directory *\n"
            )),
            "the record names the bundle and what was placed outside the prefix"
        );
        // `bin/` and `lib/` are replaced and refilled by the one copy into the
        // prefix, not file by file.
        assert!(env.ran(&["rm", "-rf", &layout.bin(), &layout.lib()]));
        assert!(env.ran(&["mkdir", "-p", p]));
        assert!(!env.ran(&["mkdir", "-p", &layout.bin()]));
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
        assert!(env.file("/root/.local/share/lodge/injected").is_some());
    }
}
