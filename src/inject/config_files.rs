//! Injecting config files and directories.

use anyhow::{Context, Result};

use super::installed::{Decision, Kind, Piece, decide, record};
use crate::bundle::manifest::{ConfigTree, Tool};
use crate::bundle::source::Prepared;
use crate::layout::Layout;
use crate::output::info;
use crate::runtime::{ContainerUser, Env};

pub(super) fn inject_config(
    env: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    bundle: &Prepared,
    tool: &Tool,
    config: &ConfigTree,
) -> Result<()> {
    let name = &tool.name;
    let dest = user.expand_home(&config.dest);
    let dir = layout.installed();
    let piece = Piece {
        tool: name,
        kind: Kind::Config,
    };

    // Asked strictly: a wrong "missing" would replace a tree the user
    // protected with `skip_if_exists`.
    let decision = decide(
        env,
        &dir,
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
        return Ok(());
    }

    info!("injecting {name} config → {dest}");

    // Replace anything at the destination before copying: a directory is merged
    // into an existing one, so removing first makes the copy mirror the bundle
    // exactly (and drops files that are no longer in it). The copy makes every
    // missing directory down to the destination as the user, so the user owns the
    // whole chain and can create siblings (nvim writing `.local/state` next to
    // `.local/share`) later.
    let written = env
        .exec(&["rm", "-rf", &dest], &crate::runtime::ExecOpts::default())
        .and_then(|()| env.cp_in(&bundle.config(name).to_string_lossy(), &dest));
    written.with_context(|| {
        format!(
            "{name}: writing {dest} as {} failed. lodge never uses root, so a target must be \
             somewhere that user can write (under ~, say)",
            user.name
        )
    })?;

    record(env, &dir, piece, &config.hash, &dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::{FORMAT, Manifest};
    use crate::bundle::source::Origin;
    use crate::runtime::fake::{FakeEnv, Who};
    use crate::tmpfile::TempDir;

    const HASH: &str = "00000000000000aa";

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
            git_safe_directories: vec![],
        };
        let prepared = Prepared {
            dir: dir.path().to_path_buf(),
            manifest,
            origin: Origin::Packed,
        };
        (dir, prepared)
    }

    fn inject(env: &FakeEnv, b: &Prepared) -> Result<()> {
        let tool = &b.manifest.tools[0];
        let user = dev();
        inject_config(
            &crate::runtime::Acting::new(env, &user),
            &user,
            &layout(),
            b,
            tool,
            tool.config.as_ref().unwrap(),
        )
    }

    #[test]
    fn a_config_lands_in_the_users_home_owned_by_them_without_any_chown() {
        let (_dir, b) = bundle("~/.config/hello", false);
        let env = FakeEnv::new();
        inject(&env, &b).unwrap();

        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("set x")
        );
        assert_eq!(
            env.owner_of("/home/dev/.config/hello/rc"),
            Some((1000, 1000))
        );
        assert_eq!(
            env.ran_as(&["mkdir", "-p", "/home/dev/.config/hello"]),
            Some(Who::Named("dev".into())),
            "the chain down to the destination is made as the user"
        );
        assert!(!env.ran_matching(|c| c[0] == "chown"));
        assert_eq!(
            env.file("/p/installed/hello.config").unwrap().trim(),
            format!("{HASH}\n/home/dev/.config/hello")
        );
    }

    #[test]
    fn nothing_here_runs_as_root() {
        let (_dir, b) = bundle("~/.config/hello", false);
        let env = FakeEnv::new();
        let user = dev();
        let acting = crate::runtime::Acting::new(&env, &user);
        inject_config(
            &acting,
            &user,
            &layout(),
            &b,
            &b.manifest.tools[0],
            b.manifest.tools[0].config.as_ref().unwrap(),
        )
        .unwrap();
        for (call, who) in env.calls() {
            assert_ne!(who, Who::Root, "{call:?} ran as root");
        }
    }

    #[test]
    fn an_unchanged_config_is_left_alone_and_a_changed_one_replaced() {
        let (_dir, b) = bundle("~/.config/hello", false);
        let env = FakeEnv::new()
            .with_file("/home/dev/.config/hello/rc", "mine")
            .with_file("/p/installed/hello.config", &format!("{HASH}\n"));
        inject(&env, &b).unwrap();
        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("mine")
        );

        let env = FakeEnv::new()
            .with_file("/home/dev/.config/hello/rc", "mine")
            .with_file("/p/installed/hello.config", "1111111111111111\n");
        inject(&env, &b).unwrap();
        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("set x")
        );
    }

    #[test]
    fn a_protected_config_is_kept_even_when_the_bundle_changed() {
        let (_dir, b) = bundle("~/.config/hello", true);
        let env = FakeEnv::new()
            .with_file("/home/dev/.config/hello/rc", "mine")
            .with_file("/p/installed/hello.config", "1111111111111111\n");
        inject(&env, &b).unwrap();
        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("mine")
        );
    }

    #[test]
    fn a_target_the_user_cannot_write_is_an_error_that_says_so() {
        let (_dir, b) = bundle("/etc/hello", false);
        let env = FakeEnv::new().with_failure(&["rm", "-rf", "/etc/hello"]);
        let err = format!("{:#}", inject(&env, &b).unwrap_err());
        assert!(
            err.contains("never uses root") && err.contains("/etc/hello") && err.contains("dev"),
            "{err}"
        );
        assert!(env.file("/p/installed/hello.config").is_none());
    }
}
