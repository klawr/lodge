//! The user's `config.toml`: what to inject, how to open sessions, which
//! secrets to deliver. Loaded once per run and validated as a whole, so every
//! mistake in it is reported together, before anything starts.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub mod secret;

use crate::shell::{container_path_problem, is_alias_name, is_plain_name};
use secret::SecretDecl;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Config {
    /// A bundle file to inject from instead of packing the `[[inject]]` entries.
    /// A relative path is relative to this file's folder.
    #[serde(default)]
    pub bundle: Option<PathBuf>,
    /// Tools to inject: a binary, a config file or directory, or both.
    #[serde(default)]
    pub inject: Vec<Injectable>,
    /// How the interactive session is opened.
    #[serde(default)]
    pub session: SessionConfig,
    /// Git settings applied in the container.
    #[serde(default)]
    pub git: GitConfig,
    /// Shell aliases, written to `/etc/profile.d/lodge.sh` (login shells).
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Secrets to deliver at attach time. Local config only, never in a bundle.
    #[serde(default)]
    pub secret: Vec<SecretDecl>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Injectable {
    /// Identifier; used for default paths.
    pub name: String,
    /// Host path to the binary to inject.
    pub binary: Option<PathBuf>,
    /// Host path to a config file or directory to inject.
    pub config: Option<PathBuf>,
    /// Where the wrapper goes in the container (default: /usr/local/bin/<name>).
    pub target_binary: Option<String>,
    /// Where the config goes in the container (default: ~/.config/<name>).
    pub target_config: Option<String>,
    /// If true, never replace the binary/config once it exists in the container,
    /// even when the bundle has a newer version (for a tree you update inside the
    /// container yourself, such as installed plugins). The default, false, keeps
    /// it current: replaced when the bundle changed, left alone when it did not.
    #[serde(default)]
    pub skip_if_exists: bool,
    /// Pack the shared libraries `ldd` finds for the binary (default: true).
    #[serde(default = "default_true")]
    pub copy_deps: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SessionConfig {
    /// Shell to launch inside the container (default: /bin/bash).
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Terminal multiplexer to wrap the session in (default: tmux).
    #[serde(default)]
    pub multiplexer: Multiplexer,
}

/// What the session is wrapped in on this machine.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub enum Multiplexer {
    /// A per-container tmux session (falls back to `none` without tmux).
    #[default]
    Tmux,
    /// Exec straight into the container.
    None,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            multiplexer: Multiplexer::default(),
        }
    }
}

fn default_shell() -> String {
    "/bin/bash".into()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GitConfig {
    /// Directories to register as git `safe.directory` inside the container.
    /// Injected plugins and the mounted workspace are owned by the host user but
    /// git may run as another user in the container, so without this git refuses
    /// them with "detected dubious ownership". Default: ["*"]; [] disables it.
    #[serde(default = "default_safe_directories")]
    pub safe_directories: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            safe_directories: default_safe_directories(),
        }
    }
}

fn default_safe_directories() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_true() -> bool {
    true
}

pub fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if (s.starts_with("~/") || s == "~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(s.get(2..).unwrap_or(""));
    }
    path
}

/// `~/.config/lodge/config.toml` (or the platform's equivalent); an error
/// when this system has no config directory (give the file with `-c`).
pub fn default_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("this system has no config directory; give the config with -c")?
        .join("lodge")
        .join("config.toml"))
}

/// The config file for this run: `-c FILE`, which must exist, or the default
/// location, which may not. Absolute, so a relative `bundle =` resolves the same
/// wherever lodge runs from.
pub fn path_for(flag: Option<PathBuf>) -> Result<PathBuf> {
    let Some(path) = flag else {
        return default_path();
    };
    let path = expand_tilde(path);
    if !path.is_file() {
        anyhow::bail!("config file {} does not exist", path.display());
    }
    Ok(std::path::absolute(&path).unwrap_or(path))
}

/// A relative path in the config is relative to the config file's folder.
fn resolve_against(config_file: &Path, path: PathBuf) -> PathBuf {
    let path = expand_tilde(path);
    if path.is_absolute() {
        return path;
    }
    config_file
        .parent()
        .map_or(path.clone(), |dir| dir.join(&path))
}

impl Config {
    /// Reads and validates the config at `path`. A missing file is an empty
    /// config: nothing is injected, and you still get a shell.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        Self::parse(&raw, path)
    }

    /// Parses `raw` as if read from `path` (which relative paths resolve against).
    pub fn parse(raw: &str, path: &Path) -> Result<Self> {
        let mut cfg: Config = toml::from_str(raw).map_err(|e| {
            anyhow::anyhow!("{}{}", e.message(), location(raw, &e))
                .context(format!("invalid config {}", path.display()))
        })?;
        cfg.bundle = cfg.bundle.take().map(|b| resolve_against(path, b));
        for item in &mut cfg.inject {
            item.binary = item.binary.take().map(expand_tilde);
            item.config = item.config.take().map(expand_tilde);
        }
        for decl in &mut cfg.secret {
            if let secret::Source::File(p) = &mut decl.from {
                *p = expand_tilde(std::mem::take(p));
            }
        }
        cfg.validate()
            .with_context(|| format!("invalid config {}", path.display()))?;
        Ok(cfg)
    }

    /// Checks what the types can't, and reports every problem at once.
    fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        let mut seen = HashSet::new();
        for item in &self.inject {
            if item.name.trim().is_empty() {
                problems.push("an [[inject]] entry has an empty `name`".to_string());
            } else if !is_plain_name(&item.name) {
                problems.push(format!(
                    "[[inject]] `{}` is not usable as a name (letters, digits and `-_.+` only)",
                    item.name
                ));
            } else if !seen.insert(item.name.as_str()) {
                problems.push(format!(
                    "[[inject]] `{}` is listed more than once (names must be unique)",
                    item.name
                ));
            }
            for (key, target) in [
                ("target_binary", &item.target_binary),
                ("target_config", &item.target_config),
            ] {
                if let Some(why) = target.as_deref().and_then(container_path_problem) {
                    problems.push(format!("[[inject]] `{}`: `{key}` {why}", item.name));
                }
            }
            if item.binary.is_none() && item.config.is_none() {
                problems.push(format!(
                    "[[inject]] `{}` sets neither `binary` nor `config`, so there is nothing to inject",
                    item.name
                ));
            }
        }
        for name in self.aliases.keys() {
            if !is_alias_name(name) {
                problems.push(format!(
                    "[aliases] `{name}` is not a valid alias name (letters, digits, `_`, `-`, `.`)"
                ));
            }
        }
        problems.extend(secret::problems(&self.secret));
        match problems.len() {
            0 => Ok(()),
            1 => anyhow::bail!("{}", problems[0]),
            _ => anyhow::bail!("\n  - {}", problems.join("\n  - ")),
        }
    }
}

/// " (line N)" plus a "did you mean" hint for an unknown key, from a toml error.
fn location(raw: &str, e: &toml::de::Error) -> String {
    let mut out = String::new();
    if let Some(span) = e.span() {
        let line = raw[..span.start.min(raw.len())].matches('\n').count() + 1;
        out.push_str(&format!(" (line {line})"));
    }
    if let Some(hint) = did_you_mean(e.message()) {
        out.push_str(&format!("; did you mean `{hint}`?"));
    }
    out
}

/// For serde's "unknown field `x`, expected one of `a`, `b`" (or "expected
/// `a`"), the expected name closest to `x`, if it is close enough to be a typo.
fn did_you_mean(message: &str) -> Option<String> {
    let rest = message.strip_prefix("unknown field `")?;
    let (unknown, expected) = rest.split_once('`')?;
    expected
        .split('`')
        .skip(1)
        .step_by(2)
        .map(|candidate| (edit_distance(unknown, candidate), candidate))
        .filter(|(d, c)| *d <= 2.max(c.len() / 3))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c.to_string())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Config> {
        Config::parse(toml, Path::new("/srv/lodge/config.toml"))
    }

    #[test]
    fn empty_config_uses_defaults() {
        let c = parse("").unwrap();
        assert!(c.inject.is_empty());
        assert_eq!(c.session.shell, "/bin/bash");
        assert_eq!(c.session.multiplexer, Multiplexer::Tmux);
        assert_eq!(c.git.safe_directories, vec!["*".to_string()]);
        assert!(c.aliases.is_empty());
    }

    #[test]
    fn parses_full_config() {
        let c = parse(
            r#"
            [[inject]]
            name = "nvim"
            binary = "/usr/bin/nvim"
            skip_if_exists = false

            [session]
            shell = "/bin/zsh"
            multiplexer = "none"

            [git]
            safe_directories = ["/work"]

            [aliases]
            ll = "ls -la"
        "#,
        )
        .unwrap();
        assert_eq!(c.inject.len(), 1);
        assert_eq!(c.inject[0].name, "nvim");
        assert!(!c.inject[0].skip_if_exists);
        assert!(c.inject[0].copy_deps); // defaults to true
        assert_eq!(c.session.shell, "/bin/zsh");
        assert_eq!(c.session.multiplexer, Multiplexer::None);
        assert_eq!(c.git.safe_directories, vec!["/work".to_string()]);
        assert_eq!(c.aliases.get("ll").map(String::as_str), Some("ls -la"));
    }

    #[test]
    fn a_relative_bundle_is_relative_to_the_config_file() {
        let cfg = Path::new("/srv/deploy/config.toml");
        assert_eq!(
            resolve_against(cfg, "bundle.tar".into()),
            Path::new("/srv/deploy/bundle.tar")
        );
        assert_eq!(
            resolve_against(cfg, "/abs/x.tar".into()),
            Path::new("/abs/x.tar")
        );
        assert_eq!(
            parse("bundle = \"b.tar\"").unwrap().bundle.unwrap(),
            Path::new("/srv/lodge/b.tar")
        );
    }

    #[test]
    fn parses_secret_declarations() {
        let c = parse(
            "[[secret]]\nname = \"GH_TOKEN\"\nfrom = { command = \"gh auth token\" }\nas = \"env\"\n",
        )
        .unwrap();
        assert_eq!(c.secret.len(), 1);
    }

    #[test]
    fn host_paths_expand_the_tilde() {
        let c = parse(
            "[[inject]]\nname = \"x\"\nbinary = \"~/bin/x\"\n\
             [[secret]]\nname = \"T\"\nfrom = { file = \"~/tok\" }\nas = \"env\"\n",
        )
        .unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            c.inject[0].binary.as_deref(),
            Some(home.join("bin/x").as_path())
        );
        assert_eq!(c.secret[0].from, secret::Source::File(home.join("tok")));
    }

    #[test]
    fn a_typo_is_an_error_with_its_line_and_a_suggestion() {
        let err = format!(
            "{:#}",
            parse("[[inject]]\nname = \"x\"\nbinary = \"/x\"\nskip_if_exist = true\n").unwrap_err()
        );
        assert!(err.contains("unknown field `skip_if_exist`"), "{err}");
        assert!(err.contains("line 4"), "{err}");
        assert!(err.contains("did you mean `skip_if_exists`?"), "{err}");

        let err = format!(
            "{:#}",
            parse("[sesion]\nshell = \"/bin/sh\"\n").unwrap_err()
        );
        assert!(err.contains("did you mean `session`?"), "{err}");
    }

    #[test]
    fn an_unknown_multiplexer_is_rejected() {
        let err = format!(
            "{:#}",
            parse("[session]\nmultiplexer = \"screen\"\n").unwrap_err()
        );
        assert!(err.contains("screen"), "{err}");
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let err = format!(
            "{:#}",
            parse(
                "[[inject]]\nname = \"a\"\nbinary = \"/a\"\n\
                 [[inject]]\nname = \"a\"\nbinary = \"/b\"\n\
                 [[inject]]\nname = \"c\"\n\
                 [[inject]]\nname = \"../up\"\nbinary = \"/u\"\n\
                 [[inject]]\nname = \"t\"\nbinary = \"/t\"\ntarget_config = \"rel/x\"\n\
                 [aliases]\n\"rm -rf\" = \"x\"\n"
            )
            .unwrap_err()
        );
        assert!(err.contains("`a` is listed more than once"), "{err}");
        assert!(err.contains("`../up` is not usable as a name"), "{err}");
        assert!(err.contains("`c` sets neither"), "{err}");
        assert!(
            err.contains("`t`: `target_config` must be absolute"),
            "{err}"
        );
        assert!(err.contains("`rm -rf` is not a valid alias name"), "{err}");
    }

    #[test]
    fn suggestions_only_for_near_misses() {
        assert_eq!(
            did_you_mean("unknown field `binay`, expected one of `name`, `binary`"),
            Some("binary".into())
        );
        assert_eq!(
            did_you_mean("unknown field `zzzzzz`, expected one of `name`, `binary`"),
            None
        );
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}
