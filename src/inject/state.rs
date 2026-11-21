//! What an injection records in the prefix, in one file ([`Layout::record`]):
//! which bundle was injected, and what lodge placed *outside* the prefix, so
//! `ldg status` can show the first and `ldg clean` can act on the second.
//!
//! One line per fact: `bundle <id>`, `wrapper <path>`, `config <path>`,
//! `safe_directory <dir>` (a git `safe.directory` entry lodge added).

use anyhow::Result;

use crate::layout::Layout;
use crate::runtime::{Container, ContainerUser, Env};

/// What was last injected for a user: where, which bundle, and what was placed
/// outside the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub layout: Layout,
    pub bundle: String,
    /// Wrapper scripts the config asked for.
    pub wrappers: Vec<String>,
    /// Config files and directories copied into place.
    pub configs: Vec<String>,
    /// Git `safe.directory` entries lodge added (not ones that were already there).
    pub safe_directories: Vec<String>,
}

fn render(installed: &Installed) -> String {
    let mut text = format!("bundle {}\n", installed.bundle);
    for path in &installed.wrappers {
        text.push_str(&format!("wrapper {path}\n"));
    }
    for path in &installed.configs {
        text.push_str(&format!("config {path}\n"));
    }
    for dir in &installed.safe_directories {
        text.push_str(&format!("safe_directory {dir}\n"));
    }
    text
}

/// The record in `text`, `None` when it names no bundle (an injection that never
/// finished leaves no record at all, so this is only a damaged file).
fn parse(layout: &Layout, text: &str) -> Option<Installed> {
    let mut installed = Installed {
        layout: layout.clone(),
        bundle: String::new(),
        wrappers: Vec::new(),
        configs: Vec::new(),
        safe_directories: Vec::new(),
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "bundle" => installed.bundle = value.to_string(),
            "wrapper" => installed.wrappers.push(value.to_string()),
            "config" => installed.configs.push(value.to_string()),
            "safe_directory" => installed.safe_directories.push(value.to_string()),
            _ => {}
        }
    }
    (!installed.bundle.is_empty()).then_some(installed)
}

/// Records an injection that has finished.
pub(super) fn record(env: &dyn Env, installed: &Installed) -> Result<()> {
    env.write_file(&installed.layout.record(), render(installed).as_bytes())
}

/// The record in `layout`'s prefix, read as the environment's actor: what an
/// earlier injection placed. `None` if there is none.
pub(super) fn previous(env: &dyn Env, layout: &Layout) -> Result<Option<Installed>> {
    Ok(env
        .read_file(&layout.record())?
        .and_then(|text| parse(layout, &text)))
}

/// What was last injected for `user` into `container` (running or stopped),
/// `None` if nothing was; an error if that can't be read. Looked for where
/// injection puts it, the user's home first and then the fallback.
pub fn installed(container: &dyn Container, user: &ContainerUser) -> Result<Option<Installed>> {
    for layout in Layout::candidates(user) {
        if let Some(text) = container.read_stored_file(&layout.record())?
            && let Some(found) = parse(&layout, &text)
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeEnv, FakeRuntime};

    fn user() -> ContainerUser {
        ContainerUser {
            requested: None,
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    fn sample() -> Installed {
        Installed {
            layout: Layout::new("/p"),
            bundle: "abc".into(),
            wrappers: vec!["/home/dev/.local/bin/nvim".into()],
            configs: vec!["/home/dev/.config/nvim".into(), "/home/dev/.hellorc".into()],
            safe_directories: vec!["/w".into()],
        }
    }

    #[test]
    fn a_record_round_trips() {
        let installed = sample();
        assert_eq!(
            parse(&installed.layout, &render(&installed)),
            Some(installed)
        );
    }

    #[test]
    fn a_record_without_a_bundle_is_no_record() {
        assert_eq!(parse(&Layout::new("/p"), "config /x\n"), None);
        assert_eq!(parse(&Layout::new("/p"), ""), None);
    }

    #[test]
    fn what_is_installed_is_found_in_the_home_or_the_fallback_prefix() {
        use crate::runtime::Runtime;
        let rt = FakeRuntime::new(FakeEnv::new)
            .with_container(
                "a",
                FakeEnv::new().with_file("/home/dev/.local/share/lodge/injected", "bundle abc\n"),
            )
            .with_container(
                "b",
                FakeEnv::new().with_file("/tmp/lodge-1000/injected", "bundle def\n"),
            )
            .with_container("c", FakeEnv::new());
        let found = |name: &str| installed(rt.container(name).as_ref(), &user()).unwrap();
        let a = found("a").unwrap();
        assert_eq!(a.bundle, "abc");
        assert_eq!(a.layout.prefix(), "/home/dev/.local/share/lodge");
        let b = found("b").unwrap();
        assert_eq!(b.bundle, "def");
        assert_eq!(b.layout.prefix(), "/tmp/lodge-1000");
        assert_eq!(found("c"), None);
    }

    #[test]
    fn the_previous_record_is_read_back() {
        let env = FakeEnv::new();
        assert_eq!(previous(&env, &Layout::new("/p")).unwrap(), None);
        record(&env, &sample()).unwrap();
        assert_eq!(previous(&env, &Layout::new("/p")).unwrap(), Some(sample()));
    }

    #[test]
    fn recording_writes_the_file_in_the_prefix() {
        let env = FakeEnv::new();
        record(&env, &sample()).unwrap();
        assert_eq!(
            env.file("/p/injected").unwrap(),
            "bundle abc\nwrapper /home/dev/.local/bin/nvim\n\
             config /home/dev/.config/nvim\nconfig /home/dev/.hellorc\n\
             safe_directory /w\n"
        );
    }
}
