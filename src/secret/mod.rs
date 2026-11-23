//! Secrets that must not be in a bundle: the value type that carries a resolved
//! secret, resolving one on this machine, and matching a declaration's
//! `targets`. The declarations themselves (`[[secret]]`) are config, in
//! [`crate::config::secret`]. [`place`] puts values into a container and
//! [`deliver`] does both for one container. See `docs/bundles-and-secrets.md`.

pub mod deliver;
pub mod place;
pub mod registry;

use anyhow::{Context, Result, bail};
use std::fmt;
use std::time::Duration;

use crate::config::secret::{SecretDecl, Source};
use crate::local::Local;

/// A resolved secret value. It cannot be printed: `Debug` redacts it and there is
/// no `Display`. The only way to the bytes is the explicit `expose`.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// True if `text` matches `pattern`, where `*` is any run of characters and `?`
/// any one. Nothing else is special.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                backtrack = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match backtrack {
                Some((bp, bt)) => {
                    backtrack = Some((bp, bt + 1));
                    pi = bp + 1;
                    ti = bt + 1;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

impl SecretDecl {
    /// Whether this secret may go to a container reached via `hosts` (the names
    /// the host answers to: `local`, or the `--remote` value and its host part)
    /// named `container`. No `targets` means anywhere.
    pub fn applies_to(&self, hosts: &[String], container: &str) -> bool {
        self.targets.is_empty()
            || self
                .targets
                .iter()
                .any(|g| glob_match(g, container) || hosts.iter().any(|h| glob_match(g, h)))
    }
}

/// How long a `command` source may run before it is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Reads a secret's value on this machine. Errors name the secret and its source
/// but never contain the value.
pub fn resolve(decl: &SecretDecl, local: &dyn Local) -> Result<Secret> {
    let (what, bytes) = match &decl.from {
        Source::Env(var) => {
            let v = local.var(var).map(crate::platform::os_string_bytes);
            (
                format!("environment variable {var}"),
                v.context("is not set"),
            )
        }
        Source::File(path) => {
            let r = local
                .read_file(path)
                .map(|mut b| {
                    // A trailing newline is an editor artefact, not part of the value.
                    while matches!(b.last(), Some(b'\n' | b'\r')) {
                        b.pop();
                    }
                    b
                })
                .map_err(|e| anyhow::anyhow!("{}", e.kind()));
            (format!("file {}", path.display()), r)
        }
        Source::Store(name) => {
            let r = local
                .store()
                .get(name)
                .and_then(|v| v.context("is not set; add it with `ldg secret set`"));
            (format!("store entry {name}"), r)
        }
        Source::Command(cmd) => {
            let program = cmd.split_whitespace().next().unwrap_or_default();
            let r = local
                .run_sh(cmd, COMMAND_TIMEOUT)
                .map(|b| b.trim_ascii().to_vec());
            (format!("command `{program}`"), r)
        }
    };
    let bytes = bytes.with_context(|| format!("secret {}: {what}", decl.name))?;
    if bytes.is_empty() {
        bail!("secret {}: {what} produced an empty value", decl.name);
    }
    Ok(Secret::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret::As;
    use crate::local::fake::FakeLocal;

    #[test]
    fn secret_value_is_redacted() {
        let s = Secret::new(b"hunter2".to_vec());
        assert!(!format!("{s:?}").contains("hunter2"));
        assert_eq!(s.expose(), b"hunter2");
    }

    fn decl(from: Source) -> SecretDecl {
        SecretDecl {
            name: "TOKEN".into(),
            from,
            as_: As::Env,
            targets: vec![],
        }
    }

    #[test]
    fn resolves_env_untouched() {
        let d = decl(Source::Env("X".into()));
        let set = FakeLocal::new().with_var("X", "  v al \n");
        assert_eq!(resolve(&d, &set).unwrap().expose(), b"  v al \n");
        let e = resolve(&d, &FakeLocal::new()).unwrap_err();
        assert!(format!("{e:#}").contains("TOKEN") && format!("{e:#}").contains("not set"));
        let e = resolve(&d, &FakeLocal::new().with_var("X", "")).unwrap_err();
        assert!(format!("{e:#}").contains("empty"));
    }

    #[test]
    fn resolves_file_stripping_trailing_newline() {
        let d = decl(Source::File("/tok".into()));
        let s = resolve(&d, &FakeLocal::new().with_file("/tok", b"abc\r\n\n")).unwrap();
        assert_eq!(s.expose(), b"abc");
        assert!(resolve(&d, &FakeLocal::new().with_file("/tok", b"\n")).is_err());
        let e = resolve(&d, &FakeLocal::new()).unwrap_err();
        assert!(format!("{e:#}").contains("not found"), "{e:#}");
    }

    #[test]
    fn command_output_is_trimmed_and_errors_hide_the_value() {
        let script = "pass show hunter2-entry --flag";
        let d = decl(Source::Command(script.into()));
        let s = resolve(&d, &FakeLocal::new().with_script(script, b"\n tok \n")).unwrap();
        assert_eq!(s.expose(), b"tok");
        let e = resolve(&d, &FakeLocal::new().with_script(script, b" \n")).unwrap_err();
        assert!(format!("{e:#}").contains("empty"));
        let e = resolve(
            &d,
            &FakeLocal::new().with_failing_script(script, "exit status: 1"),
        )
        .unwrap_err();
        let msg = format!("{e:#}");
        // Only the program is named, not its arguments.
        assert!(
            msg.contains("`pass`") && !msg.contains("hunter2-entry"),
            "{msg}"
        );
    }

    #[test]
    fn resolves_a_store_entry_and_says_what_is_wrong_when_it_cannot() {
        let d = decl(Source::Store("tok".into()));
        let have = FakeLocal::new().with_stored("tok", b"s3cret");
        assert_eq!(resolve(&d, &have).unwrap().expose(), b"s3cret");
        // Missing: the message says how to add it, and names no value.
        let e = format!("{:#}", resolve(&d, &FakeLocal::new()).unwrap_err());
        assert!(e.contains("TOKEN") && e.contains("ldg secret set"), "{e}");
        // A store that cannot be used is reported, not taken for "not set".
        let e = format!(
            "{:#}",
            resolve(&d, &FakeLocal::new().with_unusable_store()).unwrap_err()
        );
        assert!(e.contains("no credential store"), "{e}");
        assert!(!e.contains("is not set"), "{e}");
    }

    #[test]
    fn globs_match_star_and_question_only() {
        assert!(glob_match("work-*", "work-db"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(glob_match("web-?", "web-1"));
        assert!(!glob_match("web-?", "web-12"));
        assert!(!glob_match("work-*", "home-work-db"));
        assert!(!glob_match("a", "A"));
        assert!(glob_match("build.host", "build.host"));
        assert!(!glob_match("build.host", "buildxhost"));
    }

    #[test]
    fn targets_match_the_container_or_any_host_name() {
        let mut d = decl(Source::Env("X".into()));
        let hosts = vec!["me@build.example".to_string(), "build.example".to_string()];
        assert!(d.applies_to(&hosts, "anything")); // no targets: anywhere
        d.targets = vec!["build.*".into()];
        assert!(d.applies_to(&hosts, "c1"));
        assert!(!d.applies_to(&["local".into()], "c1"));
        d.targets = vec!["proj-*".into(), "local".into()];
        assert!(d.applies_to(&["local".into()], "c1"));
        assert!(d.applies_to(&hosts, "proj-api-1"));
        assert!(!d.applies_to(&hosts, "other"));
    }
}
