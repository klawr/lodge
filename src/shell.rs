//! POSIX shell quoting, shared by everything that builds a command line for
//! `sh -c` or for ssh to hand to a remote login shell.

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
    use super::{shell_quote, which_in};

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
