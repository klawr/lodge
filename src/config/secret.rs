//! `[[secret]]` declarations: what the config says about a secret (its name,
//! where its value comes from, how it is presented in the container, where it
//! may go). Declarations only; resolving and placing values is [`crate::secret`].

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::shell::{container_path_problem, is_var_name};

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

/// A checked `[[secret]]` declaration, made from a [`RawSecret`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Serialize, schemars::JsonSchema))]
struct RawFrom {
    env: Option<String>,
    file: Option<PathBuf>,
    command: Option<String>,
}

/// `"env"`, or a file in the container.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[cfg_attr(test, derive(Serialize, schemars::JsonSchema))]
enum RawAs {
    Kind(String),
    File { file: String, mode: Option<String> },
}

// A `[[secret]]` entry as written in the file: what the config is parsed into
// (and its schema made from). Checked and turned into a `SecretDecl` by
// `TryFrom`, whose error says what is wrong with it. (A plain comment: doc
// comments become the schema's text.)
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    test,
    derive(Serialize, schemars::JsonSchema),
    schemars(rename = "Secret")
)]
pub struct RawSecret {
    name: String,
    from: RawFrom,
    #[serde(rename = "as")]
    as_: RawAs,
    #[serde(default)]
    targets: Vec<String>,
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

/// What is wrong across the declarations as a whole: every name declared more
/// than once (they would collide as variables and as files), each reported once.
pub fn problems(decls: &[SecretDecl]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, d) in decls.iter().enumerate() {
        let first = !decls[..i].iter().any(|o| o.name == d.name);
        if first && decls[i + 1..].iter().any(|o| o.name == d.name) {
            out.push(format!("secret {:?} is declared more than once", d.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Doc {
        secret: Vec<RawSecret>,
    }

    /// The declarations in `toml`, or the first thing wrong with them.
    fn parse(toml: &str) -> Result<Vec<SecretDecl>, String> {
        toml::from_str::<Doc>(toml)
            .map_err(|e| e.to_string())?
            .secret
            .into_iter()
            .map(SecretDecl::try_from)
            .collect()
    }

    fn err(toml: &str) -> String {
        parse(toml).unwrap_err()
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
    fn every_duplicate_name_is_reported_once() {
        let s = parse(
            "[[secret]]\nname='A'\nfrom={env='X'}\nas='env'\n\
             [[secret]]\nname='A'\nfrom={env='Y'}\nas='env'\n\
             [[secret]]\nname='A'\nfrom={env='Z'}\nas='env'\n\
             [[secret]]\nname='B'\nfrom={env='X'}\nas='env'\n\
             [[secret]]\nname='B'\nfrom={env='Y'}\nas='env'\n\
             [[secret]]\nname='C'\nfrom={env='Y'}\nas='env'",
        )
        .unwrap();
        assert_eq!(
            problems(&s),
            [
                "secret \"A\" is declared more than once",
                "secret \"B\" is declared more than once"
            ]
        );
        assert!(problems(&s[5..]).is_empty());
    }
}
