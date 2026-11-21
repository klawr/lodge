//! Injecting config files and directories.

use anyhow::{Context, Result};

use crate::bundle::manifest::{ConfigTree, Tool};
use crate::bundle::source::Prepared;
use crate::output::{info, warning};
use crate::runtime::{ContainerUser, Env};

/// Copies the tool's config to its destination and returns that path.
/// `placed_before` is what an earlier injection placed: anything else already
/// at the destination is not lodge's, so replacing it is warned about.
pub(super) fn inject_config(
    env: &dyn Env,
    user: &ContainerUser,
    bundle: &Prepared,
    tool: &Tool,
    config: &ConfigTree,
    placed_before: &[String],
) -> Result<String> {
    let name = &tool.name;
    let dest = user.expand_home(&config.dest);
    if !placed_before.contains(&dest) && env.exists(&dest)? {
        warning!("{dest} already exists and was not injected by lodge; replacing it");
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
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::Manifest;
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

    /// A bundle whose tool `hello` has the config tree `config/hello/rc`,
    /// going to `dest`.
    fn bundle(dest: &str) -> (TempDir, Prepared) {
        let dir = TempDir::new("cfg-bundle").unwrap();
        std::fs::create_dir_all(dir.path().join("config/hello")).unwrap();
        std::fs::write(dir.path().join("config/hello/rc"), "set x").unwrap();
        let tool = Tool {
            name: "hello".into(),
            binary: None,
            config: Some(ConfigTree {
                dest: dest.into(),
                is_dir: true,
                hash: "00000000000000aa".into(),
            }),
        };
        let manifest = Manifest {
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

    fn inject(env: &FakeEnv, b: &Prepared) -> Result<String> {
        inject_known(env, b, &[])
    }

    fn inject_known(env: &FakeEnv, b: &Prepared, placed_before: &[String]) -> Result<String> {
        let tool = &b.manifest.tools[0];
        let user = dev();
        inject_config(
            &crate::runtime::Acting::new(env, &user),
            &user,
            b,
            tool,
            tool.config.as_ref().unwrap(),
            placed_before,
        )
    }

    #[test]
    fn a_config_lands_in_the_users_home_owned_by_them_without_any_chown() {
        let (_dir, b) = bundle("~/.config/hello");
        let env = FakeEnv::new();
        let placed = inject(&env, &b).unwrap();

        assert_eq!(placed, "/home/dev/.config/hello");
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
        for (call, who) in env.calls() {
            assert_ne!(who, Who::Root, "{call:?} ran as root");
        }
    }

    #[test]
    fn whatever_was_there_is_replaced() {
        let (_dir, b) = bundle("~/.config/hello");
        let env = FakeEnv::new().with_file("/home/dev/.config/hello/rc", "mine");
        inject(&env, &b).unwrap();
        assert_eq!(
            env.file("/home/dev/.config/hello/rc").as_deref(),
            Some("set x")
        );
        assert!(env.ran(&["rm", "-rf", "/home/dev/.config/hello"]));
    }

    #[test]
    fn only_a_destination_lodge_did_not_place_is_warned_about() {
        let (_dir, b) = bundle("~/.config/hello");
        let present = || FakeEnv::new().with_file("/home/dev/.config/hello/rc", "mine");
        let checked = |env: &FakeEnv| {
            env.ran_matching(|c| {
                c.iter().any(|a| a.contains("hello")) && c[0] != "rm" && c[0] != "mkdir"
            })
        };
        // The existence check is made for a destination nobody recorded...
        let env = present();
        inject(&env, &b).unwrap();
        assert!(checked(&env), "an unrecorded destination was not checked");
        // ...and not for one an earlier injection placed.
        let env = present();
        inject_known(&env, &b, &["/home/dev/.config/hello".to_string()]).unwrap();
        assert!(!checked(&env), "a recorded destination was checked");
    }

    #[test]
    fn a_target_the_user_cannot_write_is_an_error_that_says_so() {
        let (_dir, b) = bundle("/etc/hello");
        let env = FakeEnv::new().with_failure(&["rm", "-rf", "/etc/hello"]);
        let err = format!("{:#}", inject(&env, &b).unwrap_err());
        assert!(
            err.contains("never uses root") && err.contains("/etc/hello") && err.contains("dev"),
            "{err}"
        );
    }
}
