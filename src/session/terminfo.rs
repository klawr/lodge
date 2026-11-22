//! Making the user's terminal work inside a container.
//!
//! `docker exec -it` does not pass the client's `TERM` on: the container always
//! sees plain `xterm`, which has 8 colours and none of the modern terminal's
//! features. So a session sets `TERM` itself, and, when the container has no
//! terminfo entry for that terminal (kitty's `xterm-kitty`, say), lodge copies
//! the entry over from the host, into the user's lodge prefix (`dir` below is its
//! `terminfo/`). A host with no terminfo database (Windows) copies nothing and
//! always gets the fallback.

use anyhow::Result;
use std::path::PathBuf;

use crate::local::Local;
use crate::output::warning;
use crate::platform;
use crate::runtime::Env;

/// What a session falls back to when the user's own terminal can't be used.
const FALLBACK: &str = "xterm-256color";

/// The local terminal's name, unless it says nothing useful.
pub fn local_term(local: &dyn Local) -> Option<String> {
    local
        .var("TERM")
        .and_then(|t| t.into_string().ok())
        // The name goes into paths and shell scripts.
        .filter(|t| t != "dumb" && crate::shell::is_plain_name(t))
}

/// The host's compiled terminfo file for `term`, as `(path relative to a
/// terminfo directory, file)`.
fn find_entry(term: &str, dirs: &[PathBuf]) -> Option<(String, PathBuf)> {
    let first = term.chars().next()?;
    let relative = format!("{first}/{term}");
    dirs.iter()
        .map(|d| d.join(&relative))
        .find(|p| p.is_file())
        .map(|p| (relative, p))
}

/// The terminfo directories ncurses searches on this host, in its order.
fn host_dirs(local: &dyn Local) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = local.var("TERMINFO") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = local.var("HOME") {
        dirs.push(PathBuf::from(home).join(".terminfo"));
    }
    if let Some(list) = local.var("TERMINFO_DIRS") {
        dirs.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    for d in [
        "/etc/terminfo",
        "/lib/terminfo",
        "/usr/lib/terminfo",
        "/usr/share/terminfo",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// Whether the container can already resolve `term`.
fn container_knows(env: &dyn Env, dir: &str, term: &str) -> bool {
    let first = term.chars().next().unwrap_or('x');
    // Best effort: a wrong "no" only copies the entry again.
    env.exists(&format!("{dir}/{first}/{term}"))
        .unwrap_or(false)
        || env.succeeds(&["infocmp", term])
}

/// The `TERM` a session should use, or `None` to leave the container's default
/// (when `term`, this machine's terminal, says nothing useful). Copies the
/// host's entry for it into the container first when the container lacks it;
/// if that isn't possible, a widely known fallback. Best effort: a missing entry
/// only costs some colours.
pub fn prepare(env: &dyn Env, dir: &str, term: Option<&str>, local: &dyn Local) -> Option<String> {
    prepare_on(platform::HAS_TERMINFO, env, dir, term, local)
}

/// [`prepare`] for a host that does or does not have a terminfo database.
fn prepare_on(
    has_terminfo: bool,
    env: &dyn Env,
    dir: &str,
    term: Option<&str>,
    local: &dyn Local,
) -> Option<String> {
    if !has_terminfo {
        return Some(FALLBACK.to_string());
    }
    prepare_from(env, dir, term?, &host_dirs(local))
}

fn prepare_from(env: &dyn Env, dir: &str, term: &str, dirs: &[PathBuf]) -> Option<String> {
    match ensure_from(env, dir, term, dirs) {
        Ok(true) => Some(term.to_string()),
        Ok(false) => Some(FALLBACK.to_string()),
        Err(e) => {
            warning!("could not copy the terminfo entry for {term}: {e:#}");
            Some(FALLBACK.to_string())
        }
    }
}

fn ensure_from(env: &dyn Env, dir: &str, term: &str, dirs: &[PathBuf]) -> Result<bool> {
    if container_knows(env, dir, term) {
        return Ok(true);
    }
    let Some((relative, file)) = find_entry(term, dirs) else {
        return Ok(false);
    };
    let bytes = std::fs::read(&file)?;
    env.write_file(&format!("{dir}/{relative}"), &bytes)?;
    Ok(true)
}

/// Shell lines for the start of a session: set `TERM`, and let ncurses find
/// entries copied by [`prepare`]. The trailing `:` keeps the system directories
/// in the search path.
pub fn session_setup(dir: &str, term: &str) -> String {
    format!(
        "export TERM={}; export TERMINFO_DIRS={}:\"${{TERMINFO_DIRS:-}}\"; ",
        crate::shell::shell_quote(term),
        crate::shell::shell_quote(dir)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::fake::FakeLocal;
    use crate::runtime::fake::FakeEnv;

    const DIR: &str = "/p/terminfo";

    /// A host terminfo directory holding a compiled entry for `term`.
    fn host_with(term: &str) -> (crate::tmpfile::TempDir, PathBuf) {
        let dir = crate::tmpfile::TempDir::new(&format!("terminfo-{term}")).unwrap();
        let entry = dir
            .path()
            .join(term.chars().next().unwrap().to_string())
            .join(term);
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, b"compiled-terminfo").unwrap();
        (dir, entry)
    }

    #[test]
    fn finds_an_entry_by_first_letter_directory() {
        let (dir, entry) = host_with("xterm-testterm");
        let found = find_entry(
            "xterm-testterm",
            &[PathBuf::from("/nonexistent"), dir.path().to_path_buf()],
        );
        assert_eq!(found, Some(("x/xterm-testterm".to_string(), entry)));
        assert!(find_entry("nope", &[dir.path().to_path_buf()]).is_none());
    }

    #[test]
    fn a_missing_entry_is_copied_into_the_container() {
        let (dir, _) = host_with("xterm-copyme");
        let env = FakeEnv::new().with_failure(&["infocmp", "xterm-copyme"]);
        assert!(ensure_from(&env, DIR, "xterm-copyme", &[dir.path().to_path_buf()]).unwrap());
        assert_eq!(
            env.file("/p/terminfo/x/xterm-copyme").unwrap(),
            "compiled-terminfo"
        );
    }

    #[test]
    fn nothing_is_copied_when_the_container_already_knows_the_terminal() {
        let (dir, _) = host_with("xterm-known");
        let env = FakeEnv::new().with_output(&["infocmp", "xterm-known"], "");
        assert!(ensure_from(&env, DIR, "xterm-known", &[dir.path().to_path_buf()]).unwrap());
        assert!(env.file("/p/terminfo/x/xterm-known").is_none());
    }

    #[test]
    fn an_unknown_terminal_with_no_host_entry_falls_back() {
        let env = FakeEnv::new().with_failure(&["infocmp", "weird-term"]);
        assert_eq!(
            prepare_from(&env, DIR, "weird-term", &[]).as_deref(),
            Some("xterm-256color")
        );
    }

    #[test]
    fn the_users_terminal_is_used_when_the_container_resolves_it() {
        let env = FakeEnv::new().with_output(&["infocmp", "tmux-256color"], "");
        assert_eq!(
            prepare_from(&env, DIR, "tmux-256color", &[]).as_deref(),
            Some("tmux-256color")
        );
        assert_eq!(prepare_on(true, &env, DIR, None, &FakeLocal::new()), None);
    }

    #[test]
    fn a_host_without_terminfo_sets_the_fallback_and_copies_nothing() {
        let env = FakeEnv::new();
        let local = FakeLocal::new().with_var("TERM", "xterm-kitty");
        for term in [None, Some("xterm-kitty")] {
            assert_eq!(
                prepare_on(false, &env, DIR, term, &local).as_deref(),
                Some("xterm-256color")
            );
        }
        assert!(env.calls().is_empty(), "{:?}", env.calls());
    }

    #[test]
    fn a_copied_entry_makes_the_users_terminal_usable() {
        let (dir, _) = host_with("xterm-prepared");
        let env = FakeEnv::new().with_failure(&["infocmp", "xterm-prepared"]);
        assert_eq!(
            prepare_from(&env, DIR, "xterm-prepared", &[dir.path().to_path_buf()]).as_deref(),
            Some("xterm-prepared")
        );
        // Asked once: the copy and the choice share the answer.
        assert_eq!(
            env.calls()
                .iter()
                .filter(|(c, _)| c.first().is_some_and(|p| p == "infocmp"))
                .count(),
            1
        );
    }

    #[test]
    fn the_session_setup_sets_term_and_keeps_system_entries_searchable() {
        let s = session_setup(DIR, "xterm-kitty");
        assert!(s.contains("export TERM='xterm-kitty'"), "{s}");
        assert!(
            s.contains("TERMINFO_DIRS='/p/terminfo':\"${TERMINFO_DIRS:-}\""),
            "{s}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_terminal_and_where_its_entries_are_come_from_the_machine_lodge_runs_on() {
        let local = FakeLocal::new()
            .with_var("TERM", "xterm-kitty")
            .with_var("TERMINFO", "/opt/ti")
            .with_var("HOME", "/home/me")
            .with_var("TERMINFO_DIRS", "/a::/b");
        assert_eq!(local_term(&local).as_deref(), Some("xterm-kitty"));
        let dirs = host_dirs(&local);
        assert_eq!(
            dirs[..4],
            [
                PathBuf::from("/opt/ti"),
                PathBuf::from("/home/me/.terminfo"),
                PathBuf::from("/a"),
                PathBuf::from("/b")
            ]
        );
        // A terminal that says nothing, or something unsafe in a path, is none.
        assert_eq!(local_term(&FakeLocal::new()), None);
        assert_eq!(local_term(&FakeLocal::new().with_var("TERM", "dumb")), None);
        assert_eq!(local_term(&FakeLocal::new().with_var("TERM", "../x")), None);
    }
}
