//! POSIX shell quoting, shared by everything that builds a command line for
//! `sh -c` or for ssh to hand to a remote login shell, and the rules for names
//! that are written into scripts and paths unquoted.

/// A name that is safe as one path component and in a script: letters, digits
/// and `-_.+`, not `.` or `..`, not empty. Tool, library and terminal names.
pub fn is_plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

/// A shell variable name: letters, digits and `_`, not starting with a digit.
/// Secret names and the variables they become.
pub fn is_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Why `path` is not usable as a destination inside a container, or `None` if
/// it is: it must be absolute or start with `~/`, and contain neither `..` nor
/// control characters. Used for anything a config or a bundle places there.
pub fn container_path_problem(path: &str) -> Option<&'static str> {
    if !(path.starts_with('/') || path.starts_with("~/")) {
        Some("must be absolute or start with ~/")
    } else if path.chars().any(char::is_control) {
        Some("contains control characters")
    } else if path.split('/').any(|part| part == "..") {
        Some("must not contain ..")
    } else {
        None
    }
}

/// `s` with every character but ASCII letters and digits turned into `-`: the
/// stem of container and tmux session names.
pub fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The name of the tmux session for `label`: `lodge-<label>`, made of letters,
/// digits and `-` only (`workspace` when nothing usable is left), plus `-<tag>`
/// when given (a short hash that keeps two projects with one label apart).
pub fn session_name(label: &str, tag: Option<&str>) -> String {
    let label = slug(label);
    let label = label.trim_matches('-');
    let label = if label.is_empty() { "workspace" } else { label };
    match tag {
        Some(tag) => format!("lodge-{label}-{tag}"),
        None => format!("lodge-{label}"),
    }
}

/// The argv that runs `script` with `sh -c`, its arguments as `$1`, `$2`, …
/// (`$0` is `sh`). Every script lodge runs with arguments is started this way,
/// so they all number their arguments alike.
pub fn script_argv<'a>(script: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["sh", "-c", script, "sh"];
    argv.extend_from_slice(args);
    argv
}

/// A command in one of the two forms devcontainer.json allows.
pub enum CmdLine<'a> {
    /// A script for `sh -c`.
    Shell(&'a str),
    /// A program and its arguments, run directly.
    Argv(&'a [String]),
}

/// POSIX single-quote escaping: wrap in single quotes, and turn any embedded
/// single quote into `'\''`. Safe for interpolation into an `sh -c`/SSH string.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Finds the executable `name` on `PATH`.
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    which_in(name, &std::env::var_os("PATH")?)
}

fn which_in(name: &str, path_var: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::{
        container_path_problem, is_plain_name, is_var_name, script_argv, session_name, shell_quote,
        slug, which_in,
    };

    #[test]
    fn session_names_are_tmux_safe_and_never_empty() {
        assert_eq!(
            session_name("My App (dev)", Some("1a2b3c")),
            "lodge-My-App--dev-1a2b3c"
        );
        assert_eq!(session_name("web_1", None), "lodge-web-1");
        assert_eq!(session_name("***", Some("x")), "lodge-workspace-x");
        assert_eq!(session_name("/abc", None), "lodge-abc");
    }

    #[test]
    fn script_arguments_start_at_one() {
        let argv = script_argv("printf '%s|%s' \"$1\" \"$2\"", &["a b", "c"]);
        let out = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a b|c");
    }

    #[test]
    fn var_names_are_portable_identifiers() {
        assert!(is_var_name("GH_TOKEN") && is_var_name("_x1"));
        for bad in ["", "1X", "A-B", "A B", "A;rm", "a\nb"] {
            assert!(!is_var_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn container_paths_are_absolute_or_home_relative_and_cannot_climb() {
        assert_eq!(container_path_problem("/usr/local/bin/x"), None);
        assert_eq!(container_path_problem("~/.config/x"), None);
        assert!(
            container_path_problem("bin/x")
                .unwrap()
                .contains("absolute")
        );
        assert!(container_path_problem("/etc/../x").unwrap().contains(".."));
        assert!(container_path_problem("/a\0b").unwrap().contains("control"));
        assert!(container_path_problem("/a\nb").unwrap().contains("control"));
    }

    #[test]
    fn slugs_keep_letters_and_digits_only() {
        assert_eq!(slug("abc123DEF"), "abc123DEF");
        assert_eq!(slug("svc.dev_1:tag/x"), "svc-dev-1-tag-x");
    }

    #[test]
    fn plain_names_cannot_climb_or_inject() {
        assert!(is_plain_name("nvim") && is_plain_name("g++") && is_plain_name("libc.so.6"));
        for bad in ["", ".", "..", "a/b", "../x", "a b", "x;y"] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn finds_a_tool_that_is_on_the_path() {
        let found = which_in("sh", std::ffi::OsStr::new("/nonexistent:/bin:/usr/bin"));
        assert!(found.is_some());
    }

    #[test]
    fn a_missing_tool_is_not_found() {
        assert!(which_in("definitely-not-a-real-tool", std::ffi::OsStr::new("/bin")).is_none());
    }

    #[test]
    fn quotes_plain() {
        assert_eq!(shell_quote("abc"), "'abc'");
    }

    #[test]
    fn quotes_specials_literally() {
        assert_eq!(shell_quote("a b; rm -rf /"), "'a b; rm -rf /'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
    }

    #[test]
    fn escapes_embedded_single_quote() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
