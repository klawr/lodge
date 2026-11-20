//! Env profiles: `export` lines in `/etc/profile.d/`, so the login shells lodge
//! opens see the devcontainer's `containerEnv`/`remoteEnv` and what features
//! add.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::runtime::Env;

/// Writes containerEnv/remoteEnv to /etc/profile.d so login shells inherit it.
pub(super) fn write_env_profile(env: &dyn Env, vars: &BTreeMap<String, String>) -> Result<()> {
    if vars.is_empty() {
        return Ok(());
    }
    write_profile(env, crate::layout::PROFILE_ENV, vars)
}

/// Writes `vars` as `export K=<value>` lines to the profile script at `path`.
/// Values are quoted with [`quote_env_value`], so literals are safe and
/// variable references still expand when the profile is sourced.
pub(super) fn write_profile(
    env: &dyn Env,
    path: &str,
    vars: &BTreeMap<String, String>,
) -> Result<()> {
    let mut script = String::new();
    for (k, v) in vars {
        script.push_str(&format!("export {k}={}\n", quote_env_value(v)));
    }
    env.write_file(path, script.as_bytes())
}

// Renders an env value for a POSIX `export K=<value>` line. Literal runs are
// shell-quoted so spaces and metacharacters (`code --wait`, `a;b`, `a'b`) are
// safe, while `${VAR}` and `$VAR` references are left bare so the login shell
// still expands them — `normalize_env_value` deliberately rewrites
// `${containerEnv:X}` to `${X}` for exactly that, and feature metadata writes
// `PATH=/x/bin:$PATH`, so quoting the whole value would break both.
fn quote_env_value(v: &str) -> String {
    let mut out = String::new();
    let mut literal = String::new();
    let mut rest = v;
    while let Some(start) = rest.find('$') {
        let after = &rest[start + 1..];
        let len = if after.starts_with('{') {
            after.find('}').map(|end| end + 1)
        } else {
            let n = after
                .char_indices()
                .take_while(|&(i, c)| {
                    c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
                .count();
            (n > 0).then_some(n)
        };
        match len {
            Some(len) => {
                literal.push_str(&rest[..start]);
                if !literal.is_empty() {
                    out.push_str(&crate::shell::shell_quote(&literal));
                    literal.clear();
                }
                out.push_str(&rest[start..=start + len]); // the reference, bare
                rest = &rest[start + 1 + len..];
            }
            // A `$` that starts no reference is a literal.
            None => {
                literal.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() || out.is_empty() {
        out.push_str(&crate::shell::shell_quote(&literal));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    #[test]
    fn quote_env_value_quotes_literals_but_expands_vars() {
        // Plain literals with spaces/metacharacters get single-quoted whole.
        assert_eq!(quote_env_value("code --wait"), "'code --wait'");
        assert_eq!(quote_env_value("a;b&&c"), "'a;b&&c'");
        assert_eq!(quote_env_value("it's"), "'it'\\''s'");
        // ${VAR} references stay bare so the login shell still expands them
        // (normalize_env_value rewrites ${containerEnv:X} to ${X}).
        assert_eq!(quote_env_value("${PATH}"), "${PATH}");
        assert_eq!(quote_env_value("${PATH}:/foo"), "${PATH}':/foo'");
        assert_eq!(quote_env_value("${A} ${B}"), "${A}' '${B}");
        assert_eq!(quote_env_value("pre ${X} post"), "'pre '${X}' post'");
        // A dangling ${ with no closing brace is treated as a literal.
        assert_eq!(quote_env_value("a${b"), "'a${b'");
        // $VAR without braces, as feature metadata writes it.
        assert_eq!(quote_env_value("/x/bin:$PATH"), "'/x/bin:'$PATH");
        assert_eq!(quote_env_value("$A$B c"), "$A$B' c'");
        // A `$` that starts no name is literal.
        assert_eq!(quote_env_value("cost $5 or $"), "'cost $5 or $'");
        assert_eq!(quote_env_value(""), "''");
    }

    #[test]
    fn env_profile_quotes_literals_and_keeps_variable_references() {
        let env = FakeEnv::new();
        let vars = BTreeMap::from([
            ("EDITOR".to_string(), "code --wait".to_string()),
            ("PATH".to_string(), "${PATH}:/opt/bin".to_string()),
        ]);
        write_env_profile(&env, &vars).unwrap();
        assert_eq!(
            env.file("/etc/profile.d/lodge-env.sh").unwrap(),
            "export EDITOR='code --wait'\nexport PATH=${PATH}':/opt/bin'\n"
        );
    }

    #[test]
    fn empty_env_writes_no_profile() {
        let env = FakeEnv::new();
        write_env_profile(&env, &BTreeMap::new()).unwrap();
        assert!(env.file("/etc/profile.d/lodge-env.sh").is_none());
    }
}
