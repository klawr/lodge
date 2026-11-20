//! Secrets that must not be in a bundle: the value type that carries a resolved
//! secret, resolving one on this machine, and matching a declaration's
//! `targets`. The declarations themselves (`[[secret]]`) are config, in
//! [`crate::config::secret`]. [`place`] puts values into a container and
//! [`deliver`] does both for one container. See `docs/bundles-and-secrets.md`.

pub mod deliver;
pub mod place;

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::config::secret::{SecretDecl, Source};

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
pub fn resolve(decl: &SecretDecl) -> Result<Secret> {
    resolve_with(decl, |k| std::env::var_os(k), run_command)
}

fn run_command(script: &str) -> Result<Vec<u8>> {
    let out = local_sh_capture(script, COMMAND_TIMEOUT)?;
    if !out.status.success() {
        bail!("{}", out.status);
    }
    Ok(out.stdout)
}

/// Runs `sh -c <script>` on *this* machine with no stdin, capturing stdout and
/// letting stderr through to the terminal, and kills it after `timeout`.
///
/// A secret's source is on the machine running lodge whatever the target is
/// (`pass show x`), so this is deliberately not [`Target`](crate::target::Target)
/// — which runs on the project's host. The returned `Output` has an empty
/// `stderr`.
fn local_sh_capture(script: &str, timeout: Duration) -> Result<Output> {
    use std::io::Read;
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    crate::output::trace(&cmd);
    let mut child = cmd.spawn().context("running sh")?;
    let mut pipe = child.stdout.take().context("capturing stdout")?;
    // Drain on a thread so a chatty child can't block on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf).ok();
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for command")? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            reader.join().ok();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = reader.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

fn resolve_with(
    decl: &SecretDecl,
    env: impl Fn(&str) -> Option<OsString>,
    command: impl Fn(&str) -> Result<Vec<u8>>,
) -> Result<Secret> {
    let (what, bytes) = match &decl.from {
        Source::Env(var) => {
            let v = env(var).map(OsStringExt::into_vec);
            (
                format!("environment variable {var}"),
                v.context("is not set"),
            )
        }
        Source::File(path) => {
            let r = std::fs::read(path)
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
        Source::Command(cmd) => {
            let program = cmd.split_whitespace().next().unwrap_or_default();
            let r = command(cmd).map(|b| b.trim_ascii().to_vec());
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

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn no_cmd(_: &str) -> Result<Vec<u8>> {
        unreachable!("not a command source")
    }

    #[test]
    fn resolves_env_untouched() {
        let d = decl(Source::Env("X".into()));
        let s = resolve_with(&d, |_| Some("  v al \n".into()), no_cmd).unwrap();
        assert_eq!(s.expose(), b"  v al \n");
        let e = resolve_with(&d, no_env, no_cmd).unwrap_err();
        assert!(format!("{e:#}").contains("TOKEN") && format!("{e:#}").contains("not set"));
        let e = resolve_with(&d, |_| Some("".into()), no_cmd).unwrap_err();
        assert!(format!("{e:#}").contains("empty"));
    }

    #[test]
    fn resolves_file_stripping_trailing_newline() {
        let tmp = crate::tmpfile::TempDir::new("secret").unwrap();
        let dir = tmp.path();
        let p = dir.join("tok");
        std::fs::write(&p, "abc\r\n\n").unwrap();
        let s = resolve_with(&decl(Source::File(p.clone())), no_env, no_cmd).unwrap();
        assert_eq!(s.expose(), b"abc");
        std::fs::write(&p, "\n").unwrap();
        assert!(resolve_with(&decl(Source::File(p.clone())), no_env, no_cmd).is_err());
        let e = resolve_with(&decl(Source::File(dir.join("missing"))), no_env, no_cmd).unwrap_err();
        assert!(format!("{e:#}").contains("not found"), "{e:#}");
    }

    #[test]
    fn command_output_is_trimmed_and_errors_hide_the_value() {
        let d = decl(Source::Command("pass show hunter2-entry --flag".into()));
        let s = resolve_with(&d, no_env, |_| Ok(b"\n tok \n".to_vec())).unwrap();
        assert_eq!(s.expose(), b"tok");
        let e = resolve_with(&d, no_env, |_| Ok(b" \n".to_vec())).unwrap_err();
        assert!(format!("{e:#}").contains("empty"));
        let e = resolve_with(&d, no_env, |_| bail!("exit status: 1")).unwrap_err();
        let msg = format!("{e:#}");
        // Only the program is named, not its arguments.
        assert!(
            msg.contains("`pass`") && !msg.contains("hunter2-entry"),
            "{msg}"
        );
    }

    #[test]
    fn real_command_source_runs_and_times_out() {
        let d = decl(Source::Command("printf '  s3cret\\n'".into()));
        assert_eq!(resolve(&d).unwrap().expose(), b"s3cret");
        let e = resolve(&decl(Source::Command("exit 3".into()))).unwrap_err();
        assert!(format!("{e:#}").contains("exit"), "{e:#}");
        let e = local_sh_capture("sleep 5", Duration::from_millis(100)).unwrap_err();
        assert!(e.to_string().contains("timed out"));
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
