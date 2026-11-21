use anyhow::Result;

use crate::layout::Layout;
use crate::runtime::{Container, ContainerUser, Env};

/// Records which bundle was injected, so `ldg status` can show it.
pub(super) fn record_bundle(env: &dyn Env, layout: &Layout, id: &str) -> Result<()> {
    env.write_file(&layout.bundle_id(), format!("{id}\n").as_bytes())
}

/// What was last injected for a user: where, and which bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub layout: Layout,
    pub bundle: String,
}

/// What was last injected for `user` into `container` (running or stopped),
/// `None` if nothing was; an error if that can't be read. Looked for where
/// injection puts it, the user's home first and then the fallback.
pub fn installed(container: &dyn Container, user: &ContainerUser) -> Result<Option<Installed>> {
    for layout in Layout::candidates(user) {
        let bundle = container
            .read_stored_file(&layout.bundle_id())?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(bundle) = bundle {
            return Ok(Some(Installed { layout, bundle }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn what_is_installed_is_found_in_the_home_or_the_fallback_prefix() {
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
        let found = |name: &str| installed(rt.container(name).as_ref(), &user).unwrap();
        let a = found("a").unwrap();
        assert_eq!(a.bundle, "abc");
        assert_eq!(a.layout.prefix(), "/home/dev/.local/share/lodge");
        let b = found("b").unwrap();
        assert_eq!(b.bundle, "def");
        assert_eq!(b.layout.prefix(), "/tmp/lodge-1000");
        assert_eq!(found("c"), None);
    }

    #[test]
    fn the_bundle_id_is_recorded_in_the_prefix() {
        let layout = Layout::new("/p");
        let env = FakeEnv::new();
        record_bundle(&env, &layout, "abc").unwrap();
        assert_eq!(env.file("/p/bundle").unwrap(), "abc\n");
    }
}
