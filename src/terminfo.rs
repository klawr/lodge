//! Making the user's terminal work inside a container.
//!
//! `docker exec -it` does not pass the client's `TERM` on: the container always
//! sees plain `xterm`, which has 8 colours and none of the modern terminal's
//! features. So a session sets `TERM` itself, and, when the container has no
//! terminfo entry for that terminal (kitty's `xterm-kitty`, say), lodge copies
//! the entry over from the host.

use anyhow::Result;
use std::path::PathBuf;

use crate::runtime::Env;

/// Where copied entries live inside the container.
pub const CONTAINER_DIR: &str = "/opt/lodge/terminfo";

/// What a session falls back to when the user's own terminal can't be used.
const FALLBACK: &str = "xterm-256color";

/// The local terminal's name, unless it says nothing useful.
pub fn local_term() -> Option<String> {
    std::env::var("TERM")
        .ok()
        .filter(|t| !t.is_empty() && t != "dumb" && is_plain_name(t))
}

/// Terminal names go into paths and shell scripts; accept only the characters
/// real ones use.
fn is_plain_name(t: &str) -> bool {
    t.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
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
fn host_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("TERMINFO") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".terminfo"));
    }
    if let Some(list) = std::env::var_os("TERMINFO_DIRS") {
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
fn container_knows(env: &dyn Env, term: &str) -> bool {
    let first = term.chars().next().unwrap_or('x');
    env.path_exists(&format!("{CONTAINER_DIR}/{first}/{term}")) || env.succeeds(&["infocmp", term])
}

/// Copies the host's terminfo entry for `term` into the container, if the
/// container lacks it and the host has it. Returns whether the container can
/// now resolve `term`.
pub fn ensure(env: &dyn Env, term: &str) -> Result<bool> {
    ensure_from(env, term, &host_dirs())
}

fn ensure_from(env: &dyn Env, term: &str, dirs: &[PathBuf]) -> Result<bool> {
    if container_knows(env, term) {
        return Ok(true);
    }
    let Some((relative, file)) = find_entry(term, dirs) else {
        return Ok(false);
    };
    let bytes = std::fs::read(&file)?;
    env.write_file(&format!("{CONTAINER_DIR}/{relative}"), &bytes)?;
    Ok(true)
}

/// The `TERM` a session should use, or `None` to leave the container's default.
/// The user's own terminal when the container can resolve it, otherwise a
/// widely known fallback.
pub fn session_term(env: &dyn Env, local: Option<&str>) -> Option<String> {
    let term = local?;
    if container_knows(env, term) {
        Some(term.to_string())
    } else {
        Some(FALLBACK.to_string())
    }
}

/// Shell lines for the start of a session: set `TERM`, and let ncurses find
/// entries copied by [`ensure`]. The trailing `:` keeps the system directories
/// in the search path.
pub fn session_setup(term: &str) -> String {
    format!(
        "export TERM={}; export TERMINFO_DIRS={CONTAINER_DIR}:\"${{TERMINFO_DIRS:-}}\"; ",
        crate::shell::shell_quote(term)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

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
        assert!(ensure_from(&env, "xterm-copyme", &[dir.path().to_path_buf()]).unwrap());
        assert_eq!(
            env.file("/opt/lodge/terminfo/x/xterm-copyme").unwrap(),
            "compiled-terminfo"
        );
    }

    #[test]
    fn nothing_is_copied_when_the_container_already_knows_the_terminal() {
        let (dir, _) = host_with("xterm-known");
        let env = FakeEnv::new().with_output(&["infocmp", "xterm-known"], "");
        assert!(ensure_from(&env, "xterm-known", &[dir.path().to_path_buf()]).unwrap());
        assert!(env.file("/opt/lodge/terminfo/x/xterm-known").is_none());
    }

    #[test]
    fn an_unknown_terminal_with_no_host_entry_falls_back() {
        let env = FakeEnv::new().with_failure(&["infocmp", "weird-term"]);
        assert!(!ensure_from(&env, "weird-term", &[]).unwrap());
        assert_eq!(
            session_term(&env, Some("weird-term")).as_deref(),
            Some("xterm-256color")
        );
    }

    #[test]
    fn the_users_terminal_is_used_when_the_container_resolves_it() {
        let env = FakeEnv::new().with_output(&["infocmp", "tmux-256color"], "");
        assert_eq!(
            session_term(&env, Some("tmux-256color")).as_deref(),
            Some("tmux-256color")
        );
        assert_eq!(session_term(&env, None), None);
    }

    #[test]
    fn names_that_could_escape_a_path_or_script_are_refused() {
        assert!(is_plain_name("xterm-256color"));
        assert!(!is_plain_name("x;rm -rf /"));
        assert!(!is_plain_name("../etc"));
    }

    #[test]
    fn the_session_setup_sets_term_and_keeps_system_entries_searchable() {
        let s = session_setup("xterm-kitty");
        assert!(s.contains("export TERM='xterm-kitty'"), "{s}");
        assert!(
            s.contains("TERMINFO_DIRS=/opt/lodge/terminfo:\"${TERMINFO_DIRS:-}\""),
            "{s}"
        );
    }
}
