//! Injecting a bundle into a target, for one user.
//!
//! Everything here is done *as that user* (through [`Acting`]) into a prefix they
//! own (see [`crate::layout`]), so it behaves the same whether the user is root, an
//! ordinary user, or rootless. Root is used only to clear what an earlier
//! injection left that the user cannot remove.
//!
//! An injection always replaces what is there: the libraries and binaries are
//! read from the bundle and sent to the prefix as one tar (see [`payload`]), and
//! the data is copied over its destinations. Whether to inject at all is the caller's
//! decision (see `ldg up`), not something this module second-guesses.

mod binary;
mod data;
mod git;
mod payload;
mod prefix;
mod remove;
mod state;

use anyhow::{Context, Result};

use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::output::{info, plural};
use crate::runtime::{Acting, ContainerUser, Env, ExecOpts};
use binary::{check_architecture, place_wrapper};
use data::inject_data;
use git::configure_git_safe;
use payload::Payload;
pub use prefix::resolve as resolve_layout;
pub use remove::remove;
pub use state::{Installed, installed};

/// Injects `prepared` for `user` into the prefix `layout` (see [`resolve_layout`]):
/// libraries and binaries in one copy, then each binary's wrapper and each piece of
/// data, then git's settings. The record that says what was injected is written last, so an
/// injection that failed halfway reads as none.
pub fn run(
    container: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    prepared: &Prepared,
) -> Result<()> {
    let env = Acting::new(container, user);
    let manifest = prepared.bundle.manifest();
    check_architecture(&env, manifest)?;

    // Before anything in the container is touched: a prefix a binary cannot be
    // pointed at is refused here.
    let payload = Payload::new(&prepared.bundle, layout)?;
    let contents: Vec<String> = [
        (manifest.binaries.len(), "binary", "binaries"),
        (manifest.libs.len(), "library", "libraries"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, one, many)| plural(n, one, many))
    .collect();
    if contents.is_empty() {
        info!("injecting bundle {}", manifest.id());
    } else {
        info!(
            "injecting bundle {} ({})",
            manifest.id(),
            contents.join(", ")
        );
    }
    // What an earlier injection placed outside the prefix, to tell it from what
    // was already there.
    let previous = state::previous(&env, layout)?;
    let placed_before = previous
        .as_ref()
        .map(|p| p.data.clone())
        .unwrap_or_default();
    let safe_before = previous.map(|p| p.safe_directories).unwrap_or_default();
    // `bin/` and `lib/` are replaced as a whole, so nothing of an earlier
    // injection is left behind. (A running binary can be unlinked, but not
    // overwritten.) What an injection made before as another user (root, when
    // the container was first reached before `remoteUser` applied) is not the
    // user's to remove, so root clears it; the copy below then makes it theirs.
    let stale = ["rm", "-rf", &layout.bin(), &layout.lib()];
    let removed = env.output(&stale, &ExecOpts::default().repeatable())?;
    if !removed.success {
        env.exec(&stale, &ExecOpts::root().repeatable())
            .with_context(|| {
                format!(
                    "removing the earlier injection failed as {} ({})",
                    user.name,
                    removed.stderr.trim().lines().next().unwrap_or("no output")
                )
            })?;
    }
    env.cp_in(&payload, layout.prefix())?;

    let mut wrappers = Vec::new();
    for binary in &manifest.binaries {
        wrappers.extend(place_wrapper(&env, user, layout, binary)?);
    }
    let mut data = Vec::new();
    for (index, piece) in manifest.data.iter().enumerate() {
        data.extend(inject_data(
            &env,
            user,
            prepared,
            index,
            piece,
            &placed_before,
        )?);
    }
    let safe_directories = configure_git_safe(&env, &manifest.git_safe_directories, &safe_before)?;

    state::record(
        &env,
        &Installed {
            layout: layout.clone(),
            bundle: manifest.id(),
            source: prepared.source.clone(),
            wrappers,
            data,
            safe_directories,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{Binary, Data, Manifest};
    use crate::bundle::testing::{manifest, prepared};
    use crate::runtime::fake::{FakeEnv, Who};

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
    fn an_earlier_injection_the_user_cannot_remove_is_cleared_as_root() {
        let (_dir, bundle) = prepared(&manifest("x86_64"), &[]);
        let layout = Layout::preferred(&dev());
        let rm = ["rm", "-rf", &layout.bin(), &layout.lib()];
        let env = FakeEnv::new()
            .with_output(&["uname", "-m"], "x86_64\n")
            .with_user_failure(&rm);

        run(&env, &dev(), &layout, &bundle).unwrap();

        let calls = env.calls();
        let rms: Vec<_> = calls.iter().filter(|(c, _)| c[0] == "rm").collect();
        assert_eq!(rms.len(), 2, "first as the user, then as root");
        assert_eq!(rms[0].1, Who::Named("dev".into()));
        assert_eq!(rms[1].1, Who::Root);
    }

    #[test]
    fn a_bundle_is_injected_whole_for_a_user_with_no_root_and_recorded() {
        let manifest = Manifest {
            binaries: vec![Binary {
                name: "hello".into(),
                dest: None,
                dynamic: false,
                hash: "00000000000000aa".into(),
            }],
            data: vec![Data {
                dest: "~/.hellorc".into(),
                is_dir: false,
                hash: "00000000000000bb".into(),
                binary: Some("hello".into()),
            }],
            git_safe_directories: vec!["*".into()],
            ..manifest("x86_64")
        };
        let id = manifest.id();
        let (_dir, bundle) = prepared(
            &manifest,
            &[
                ("bin/hello", b"#!/bin/sh\necho hi\n"),
                ("data/0", b"greeting=hi"),
            ],
        );
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
                "bundle {id}\ndata /home/dev/.hellorc\nsafe_directory *\n"
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
        let manifest = Manifest {
            data: vec![Data {
                dest: "~/.c".into(),
                is_dir: false,
                hash: "00000000000000bb".into(),
                binary: None,
            }],
            ..manifest("x86_64")
        };
        let (_dir, bundle) = prepared(&manifest, &[("data/0", b"x")]);
        let env = FakeEnv::new();
        let root = ContainerUser::root();
        run(&env, &root, &Layout::preferred(&root), &bundle).unwrap();
        assert_eq!(env.file("/root/.c").as_deref(), Some("x"));
        assert!(env.file("/root/.local/share/lodge/injected").is_some());
    }
}
