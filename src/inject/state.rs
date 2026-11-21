use anyhow::Result;

use crate::layout::Layout;
use crate::output::warning;
use crate::runtime::{Container, ContainerUser, Env};

// The layout of the prefix is a format. Stamping it lets a future lodge migrate
// it, and keeps an older lodge from silently altering newer state. Version 1 was
// the root-owned `/opt/lodge`; 2 is the prefix in the user's home.
const STATE_VERSION: u32 = 2;

/// Records which bundle was injected, so `ldg status` can show it.
pub(super) fn record_bundle(env: &dyn Env, layout: &Layout, id: &str) -> Result<()> {
    env.write_file(&layout.bundle_id(), format!("{id}\n").as_bytes())
}

/// The id of the bundle last injected for `user` into `container` (running or
/// stopped), `None` if none was; an error if that can't be read. Looked for where
/// injection puts it, the user's home first and then the fallback.
pub fn installed_bundle(container: &dyn Container, user: &ContainerUser) -> Result<Option<String>> {
    for layout in Layout::candidates(user) {
        let id = container
            .read_stored_file(&layout.bundle_id())?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if id.is_some() {
            return Ok(id);
        }
    }
    Ok(None)
}

/// Stamps the format version of the injected state, warning when a newer lodge
/// wrote it.
pub(super) fn ensure_state_version(env: &dyn Env, layout: &Layout) -> Result<()> {
    let path = layout.version();
    let found = env
        .read_file(&path)?
        .and_then(|s| s.trim().parse::<u32>().ok());
    match found {
        Some(v) if v > STATE_VERSION => {
            warning!(
                "{} was set up by a newer lodge (state version {v}, \
                 this ldg understands {STATE_VERSION}); consider updating ldg",
                layout.prefix()
            );
            Ok(())
        }
        Some(_) => Ok(()),
        None => env.write_file(&path, format!("{STATE_VERSION}\n").as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn state_version_is_stamped_once() {
        let layout = Layout::new("/p");
        let env = FakeEnv::new();
        ensure_state_version(&env, &layout).unwrap();
        assert_eq!(env.file("/p/version").unwrap(), "2\n");
        // A newer stamp is left alone (with a warning), never overwritten.
        let newer = FakeEnv::new().with_file("/p/version", "7\n");
        ensure_state_version(&newer, &layout).unwrap();
        assert_eq!(newer.file("/p/version").unwrap(), "7\n");
    }

    #[test]
    fn the_installed_bundle_is_found_in_the_home_or_the_fallback_prefix() {
        use crate::runtime::Runtime;
        use crate::runtime::fake::FakeRuntime;
        let user = ContainerUser {
            requested: None,
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        };
        let rt = FakeRuntime::new(FakeEnv::new)
            .with_container(
                "a",
                FakeEnv::new().with_file("/home/dev/.local/share/lodge/bundle", "abc\n"),
            )
            .with_container(
                "b",
                FakeEnv::new().with_file("/tmp/lodge-1000/bundle", "def\n"),
            )
            .with_container("c", FakeEnv::new());
        assert_eq!(
            installed_bundle(rt.container("a").as_ref(), &user)
                .unwrap()
                .as_deref(),
            Some("abc")
        );
        assert_eq!(
            installed_bundle(rt.container("b").as_ref(), &user)
                .unwrap()
                .as_deref(),
            Some("def")
        );
        assert_eq!(
            installed_bundle(rt.container("c").as_ref(), &user).unwrap(),
            None
        );
    }

    #[test]
    fn the_bundle_id_is_recorded_in_the_prefix() {
        let layout = Layout::new("/p");
        let env = FakeEnv::new();
        record_bundle(&env, &layout, "abc").unwrap();
        assert_eq!(env.file("/p/bundle").unwrap(), "abc\n");
    }
}
