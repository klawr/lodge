//! Injecting config files and directories.

use anyhow::Result;
use std::path::Path;

use super::home::chown_home_chain;
use super::installed::{Decision, Kind, Piece, decide, record};
use crate::bundle::manifest::{ConfigTree, Tool};
use crate::bundle::source::Prepared;
use crate::output::info;
use crate::runtime::{ContainerUser, Env, ExecOpts};

pub(super) fn inject_config(
    env: &dyn Env,
    user: &ContainerUser,
    bundle: &Prepared,
    tool: &Tool,
    config: &ConfigTree,
) -> Result<()> {
    let name = &tool.name;
    let dest = user.expand_home(&config.dest);
    let piece = Piece {
        tool: name,
        kind: Kind::Config,
    };

    // Asked strictly: a wrong "missing" would replace a tree the user
    // protected with `skip_if_exists`.
    let decision = decide(
        env,
        piece,
        &config.hash,
        env.exists(&dest)?,
        tool.skip_if_exists,
    );
    if decision != Decision::Install {
        if decision == Decision::UpToDate {
            info!("{name} config is up to date");
        } else {
            info!("{name} config already present, skipping");
        }
        // Fix ownership even when skipping the copy — the files may be
        // root-owned from a previous run.
        chown_home_chain(env, user, &dest)?;
        return Ok(());
    }

    info!("injecting {name} config → {dest}");

    // Create parent dirs as the container user when the destination is under their
    // home directory — `mkdir -p` as root would leave the whole chain
    // (e.g. /home/dev/.local) root-owned, blocking the user from creating
    // siblings later. For system paths (e.g. /usr/local/share) root is correct.
    let parent = Path::new(&dest)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    if !user.is_root() && user.is_in_home(&dest) {
        env.exec(
            &["mkdir", "-p", &parent],
            &ExecOpts::as_user(user.exec_as()),
        )?;
    } else {
        env.mkdir_p(&[&parent])?;
    }

    // Replace anything at the destination before copying: `cp_in` merges a directory
    // into an existing one, so removing first makes the copy mirror the bundle
    // exactly (and drops files that are no longer in it).
    env.exec_root(&["rm", "-rf", &dest])?;
    env.cp_in(&bundle.config(name).to_string_lossy(), &dest)?;

    // Fix ownership of the entire path chain from home to the destination. docker cp
    // (especially in remote mode) may recreate intermediate dirs as root.
    // Chowning the chain ensures the user can create siblings (e.g.
    // .local/state) at runtime.
    chown_home_chain(env, user, &dest)?;

    record(env, piece, &config.hash, &dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{FORMAT, Manifest};
    use crate::bundle::source::Origin;
    use crate::runtime::fake::{FakeEnv, Who};
    use crate::tmpfile::TempDir;
    use std::collections::BTreeMap;

    const HASH: &str = "00000000000000aa";

    fn dev() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    /// A bundle whose tool `hello` has the config tree `config/hello/rc`,
    /// going to `dest`.
    fn bundle(dest: &str, skip_if_exists: bool) -> (TempDir, Prepared) {
        let dir = TempDir::new("cfg-bundle").unwrap();
        std::fs::create_dir_all(dir.path().join("config/hello")).unwrap();
        std::fs::write(dir.path().join("config/hello/rc"), "set x").unwrap();
        let tool = Tool {
            name: "hello".into(),
            binary: None,
            config: Some(ConfigTree {
                dest: dest.into(),
                is_dir: true,
                hash: HASH.into(),
            }),
            skip_if_exists,
        };
        let manifest = Manifest {
            format: FORMAT,
            lodge_version: "0".into(),
            created: 0,
            arch: "x86_64".into(),
            linker: None,
            libs: vec![],
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

    fn inject(env: &FakeEnv, user: &ContainerUser, b: &Prepared) {
        let tool = &b.manifest.tools[0];
        inject_config(env, user, b, tool, tool.config.as_ref().unwrap()).unwrap();
    }

    #[test]
    fn a_config_in_the_home_is_copied_recorded_and_owned_by_the_user() {
        let (_d, b) = bundle("~/.config/hello", false);
        let env = FakeEnv::new().with_file("/home/dev/.config/hello/stale", "old");
        inject(&env, &dev(), &b);

        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("set x")
        );
        assert!(
            env.file("/home/dev/.config/hello/stale").is_none(),
            "the copy mirrors the bundle"
        );
        assert_eq!(
            env.file("/opt/lodge/installed/hello.config").as_deref(),
            Some("00000000000000aa\n/home/dev/.config/hello\n")
        );
        // Parents are made as the user, and the chain below the home is theirs.
        assert_eq!(
            env.ran_as(&["mkdir", "-p", "/home/dev/.config"]),
            Some(Who::Named("dev".into()))
        );
        assert!(env.ran(&["chown", "1000:1000", "/home/dev/.config"]));
        assert!(env.ran(&["chown", "-R", "1000:1000", "/home/dev/.config/hello"]));
        assert!(
            !env.ran(&["chown", "1000:1000", "/home/dev"]),
            "not the home itself"
        );
    }

    #[test]
    fn a_config_outside_the_home_is_left_to_root() {
        let (_d, b) = bundle("/usr/local/share/hello", false);
        let env = FakeEnv::new();
        inject(&env, &dev(), &b);
        assert_eq!(
            env.file("/usr/local/share/hello/rc").as_deref(),
            Some("set x")
        );
        assert_eq!(
            env.ran_as(&["mkdir", "-p", "/usr/local/share"]),
            Some(Who::Root)
        );
        assert!(!env.ran_matching(|c| c[0] == "chown"));
    }

    #[test]
    fn an_unchanged_config_is_not_copied_again() {
        let (_d, b) = bundle("~/.config/hello", false);
        let env = FakeEnv::new()
            .with_file("/home/dev/.config/hello/rc", "edited in the container")
            .with_file(
                "/opt/lodge/installed/hello.config",
                "00000000000000aa\n/home/dev/.config/hello\n",
            );
        inject(&env, &dev(), &b);
        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("edited in the container")
        );
    }

    #[test]
    fn a_protected_config_is_kept_when_the_bundle_changed_but_still_owned() {
        let (_d, b) = bundle("~/.config/hello", true);
        let env = FakeEnv::new()
            .with_file("/home/dev/.config/hello/plugins", "the user's")
            .with_file(
                "/opt/lodge/installed/hello.config",
                "1111111111111111\n/home/dev/.config/hello\n",
            );
        inject(&env, &dev(), &b);
        assert_eq!(
            env.file("/home/dev/.config/hello/plugins").as_deref(),
            Some("the user's")
        );
        assert!(env.file("/home/dev/.config/hello/rc").is_none());
        assert!(env.ran(&["chown", "-R", "1000:1000", "/home/dev/.config/hello"]));
    }
}
