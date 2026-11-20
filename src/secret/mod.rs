//! Secrets that must not be in a bundle: their declarations (`[[secret]]` in
//! the local config, never in a bundle), the value type that carries a resolved
//! secret, and resolving one on this machine. [`place`] puts values into a
//! container and [`deliver`] does both for one container. See
//! `docs/bundles-and-secrets.md`.

pub mod deliver;
pub mod place;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::shell::{container_path_problem, is_var_name};

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

/// Where a secret's value comes from, on the machine running ldg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// An environment variable of the ldg process.
    Env(String),
    /// A file on the host.
    File(PathBuf),
    /// A shell command whose trimmed stdout is the value.
    Command(String),
}

/// How a secret is presented inside the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum As {
    /// An environment variable named like the secret, set in the session.
    Env,
    /// A file at `path` (container side) with permission bits `mode`.
    File { path: String, mode: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSecret")]
pub struct SecretDecl {
    pub name: String,
    #[serde(rename = "from")]
    pub from: Source,
    #[serde(rename = "as")]
    pub as_: As,
    /// Remote host or container-name globs this secret may go to; empty means anywhere.
    pub targets: Vec<String>,
}

/// Where the value comes from on this machine: exactly one of the three.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct RawFrom {
    env: Option<String>,
    file: Option<PathBuf>,
    command: Option<String>,
}

/// `"env"`, or a file in the container.
#[derive(Deserialize)]
#[serde(untagged)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
enum RawAs {
    Kind(String),
    File { file: String, mode: Option<String> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct RawSecret {
    name: String,
    from: RawFrom,
    #[serde(rename = "as")]
    as_: RawAs,
    #[serde(default)]
    targets: Vec<String>,
}

// The config schema describes what is written in the file, which is RawSecret.
#[cfg(test)]
impl schemars::JsonSchema for SecretDecl {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Secret".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        RawSecret::json_schema(generator)
    }
}

const DEFAULT_FILE_MODE: u32 = 0o600;

impl TryFrom<RawSecret> for SecretDecl {
    type Error = String;

    fn try_from(raw: RawSecret) -> Result<Self, String> {
        let name = raw.name;
        validate_name(&name).map_err(|e| e.to_string())?;
        let ctx = |e: String| format!("secret {name:?}: {e}");

        let from = match (raw.from.env, raw.from.file, raw.from.command) {
            (Some(v), None, None) => {
                validate_name(&v).map_err(|e| ctx(format!("from.env: {e}")))?;
                Source::Env(v)
            }
            // `~` is expanded by `Config::parse`, like every host path.
            (None, Some(p), None) => Source::File(p),
            (None, None, Some(c)) if !c.trim().is_empty() => Source::Command(c),
            (None, None, Some(_)) => return Err(ctx("from.command is empty".into())),
            _ => {
                return Err(ctx(
                    "`from` needs exactly one of env, file or command".into()
                ));
            }
        };

        let as_ = match raw.as_ {
            RawAs::Kind(k) if k == "env" => As::Env,
            RawAs::Kind(k) => {
                return Err(ctx(format!(
                    "unknown `as` {k:?} (use \"env\" or {{ file = \"/path\", mode = \"0600\" }})"
                )));
            }
            RawAs::File { file, mode } => {
                validate_container_path(&file).map_err(|e| ctx(format!("as.file: {e}")))?;
                let mode = match mode {
                    None => DEFAULT_FILE_MODE,
                    Some(m) => parse_mode(&m).map_err(|e| ctx(format!("as.mode: {e}")))?,
                };
                As::File { path: file, mode }
            }
        };

        if raw.targets.iter().any(|t| t.trim().is_empty()) {
            return Err(ctx("targets must not contain empty entries".into()));
        }

        Ok(SecretDecl {
            name,
            from,
            as_,
            targets: raw.targets,
        })
    }
}

/// Names reach shell scripts (as variable names), so keep them to the portable set.
fn validate_name(name: &str) -> Result<()> {
    if !is_var_name(name) {
        bail!(
            "invalid name {name:?}: use letters, digits and underscores, not starting with a digit"
        );
    }
    Ok(())
}

/// Container-side destination: absolute or `~/…`, no `..`, no control characters.
fn validate_container_path(path: &str) -> Result<()> {
    match container_path_problem(path) {
        Some(why) => bail!("{path:?} {why}"),
        None => Ok(()),
    }
}

fn parse_mode(s: &str) -> Result<u32> {
    let digits = s.strip_prefix("0o").unwrap_or(s);
    match u32::from_str_radix(digits, 8) {
        Ok(m) if m <= 0o777 => Ok(m),
        _ => bail!("{s:?} is not an octal mode up to 0777"),
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

/// Reject duplicate names (they would collide as variables and as files).
pub fn validate_all(decls: &[SecretDecl]) -> Result<()> {
    for (i, d) in decls.iter().enumerate() {
        if decls[..i].iter().any(|o| o.name == d.name) {
            bail!("secret {:?} is declared more than once", d.name);
        }
    }
    Ok(())
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

    #[derive(Deserialize)]
    struct Doc {
        secret: Vec<SecretDecl>,
    }

    fn parse(toml: &str) -> Result<Vec<SecretDecl>, toml::de::Error> {
        toml::from_str::<Doc>(toml).map(|d| d.secret)
    }

    fn err(toml: &str) -> String {
        parse(toml).unwrap_err().to_string()
    }

    #[test]
    fn parses_env_secret() {
        let s = parse(
            r#"
            [[secret]]
            name = "ANTHROPIC_API_KEY"
            from = { command = "pass show anthropic" }
            as = "env"
            targets = ["work-*"]
        "#,
        )
        .unwrap();
        assert_eq!(s[0].name, "ANTHROPIC_API_KEY");
        assert_eq!(s[0].from, Source::Command("pass show anthropic".into()));
        assert_eq!(s[0].as_, As::Env);
        assert_eq!(s[0].targets, vec!["work-*".to_string()]);
    }

    #[test]
    fn parses_file_secret_with_default_and_explicit_mode() {
        let s = parse(
            r#"
            [[secret]]
            name = "gh"
            from = { file = "/run/gh-token" }
            as = { file = "/root/.config/gh/token" }

            [[secret]]
            name = "other"
            from = { env = "OTHER_TOKEN" }
            as = { file = "~/.token", mode = "0640" }
        "#,
        )
        .unwrap();
        assert_eq!(
            s[0].as_,
            As::File {
                path: "/root/.config/gh/token".into(),
                mode: 0o600
            }
        );
        assert_eq!(
            s[1].as_,
            As::File {
                path: "~/.token".into(),
                mode: 0o640
            }
        );
        assert!(s[0].targets.is_empty());
    }

    #[test]
    fn rejects_bad_names() {
        for bad in ["", "1X", "A-B", "A B", "A;rm", "A$(x)", "a\nb"] {
            let toml =
                format!("[[secret]]\nname = {bad:?}\nfrom = {{ env = \"X\" }}\nas = \"env\"\n");
            assert!(parse(&toml).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn from_needs_exactly_one_source() {
        let e = err("[[secret]]\nname='A'\nfrom = {}\nas='env'");
        assert!(e.contains("exactly one"), "{e}");
        let e = err("[[secret]]\nname='A'\nfrom = { env='X', file='/y' }\nas='env'");
        assert!(e.contains("exactly one"), "{e}");
        let e = err("[[secret]]\nname='A'\nfrom = { command='  ' }\nas='env'");
        assert!(e.contains("empty"), "{e}");
        assert!(parse("[[secret]]\nname='A'\nfrom = { cmd='x' }\nas='env'").is_err());
    }

    #[test]
    fn rejects_bad_as() {
        let e = err("[[secret]]\nname='A'\nfrom={env='X'}\nas='socket'");
        assert!(e.contains("unknown `as`"), "{e}");
        let e = err("[[secret]]\nname='A'\nfrom={env='X'}\nas={file='rel/path'}");
        assert!(e.contains("absolute"), "{e}");
        let e = err("[[secret]]\nname='A'\nfrom={env='X'}\nas={file='/a/../b'}");
        assert!(e.contains(".."), "{e}");
        let e = err("[[secret]]\nname='A'\nfrom={env='X'}\nas={file='/a', mode='0999'}");
        assert!(e.contains("octal"), "{e}");
        let e = err("[[secret]]\nname='A'\nfrom={env='X'}\nas={file='/a', mode='1777'}");
        assert!(e.contains("octal"), "{e}");
    }

    #[test]
    fn rejects_empty_target_and_unknown_keys() {
        let e = err("[[secret]]\nname='A'\nfrom={env='X'}\nas='env'\ntargets=['']");
        assert!(e.contains("targets"), "{e}");
        assert!(parse("[[secret]]\nname='A'\nfrom={env='X'}\nas='env'\nvalue='oops'").is_err());
    }

    #[test]
    fn rejects_duplicate_names() {
        let s = parse(
            "[[secret]]\nname='A'\nfrom={env='X'}\nas='env'\n\
             [[secret]]\nname='A'\nfrom={env='Y'}\nas='env'",
        )
        .unwrap();
        assert!(validate_all(&s).is_err());
    }

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
