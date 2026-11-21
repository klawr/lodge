//! The user's `config.toml`: what to inject, how to open sessions, which
//! secrets to deliver. Loaded once per run and validated as a whole, so every
//! mistake in it is reported together, before anything starts.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub mod secret;

use crate::shell::{container_path_problem, is_plain_name};
use secret::{RawSecret, SecretDecl};

// The user's config. `S` is how a `[[secret]]` entry is held: as written
// (`RawSecret`, what the file is parsed into and the schema is made from), or
// checked (`SecretDecl`, the default, what the rest of lodge works with). A
// plain comment, not a doc comment: doc comments become the schema's text.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "S: Deserialize<'de>"))]
#[cfg_attr(
    test,
    derive(Serialize, schemars::JsonSchema),
    schemars(rename = "Config")
)]
pub struct Config<S = SecretDecl> {
    /// Where to work, unless `--target` says otherwise. Default: docker.
    #[serde(default)]
    pub target: Option<String>,
    /// What each target lets you say about it, by target name: `[targets.docker]`.
    /// The keys of a table are checked by the target that reads it.
    #[serde(default)]
    #[cfg_attr(
        test,
        schemars(with = "BTreeMap<String, serde_json::Map<String, serde_json::Value>>")
    )]
    pub targets: BTreeMap<String, toml::Table>,
    /// A bundle to inject from instead of packing the `[[inject]]` entries: a
    /// file, or a registry as `oci://ghcr.io/me/lodge:1`. A relative file path is
    /// relative to this file's folder.
    #[serde(default)]
    pub bundle: Option<String>,
    /// Tools to inject: a binary, a config file or directory, or both.
    #[serde(default)]
    pub inject: Vec<Injectable>,
    /// How the interactive session is opened.
    #[serde(default)]
    pub session: SessionConfig,
    /// Git settings applied in the container.
    #[serde(default)]
    pub git: GitConfig,
    /// Secrets to deliver at attach time. Local config only, never in a bundle.
    #[serde(default)]
    // Generic, so schemars can't work the default out itself.
    #[cfg_attr(test, schemars(extend("default" = [])))]
    pub secret: Vec<S>,
}

impl<S> Default for Config<S> {
    fn default() -> Self {
        Self {
            target: None,
            targets: BTreeMap::new(),
            bundle: None,
            inject: Vec::new(),
            session: SessionConfig::default(),
            git: GitConfig::default(),
            secret: Vec::new(),
        }
    }
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
    /// Where to put an extra wrapper script that runs the binary (default: none:
    /// the binary is on the `PATH` of the sessions lodge opens).
    pub target_binary: Option<String>,
    /// Where the config goes in the container (default: `~/.config/<name>`).
    pub target_config: Option<String>,
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
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
/// A bundle reference as the config wrote it: a path is resolved against the
/// config file's folder, anything with a scheme (`oci://…`) is not a path and is
/// kept as it is. Which scheme means what is the bundle stores' business.
fn resolve_reference(config_file: &Path, reference: String) -> String {
    if reference.contains("://") {
        return reference;
    }
    resolve_against(config_file, PathBuf::from(reference))
        .to_string_lossy()
        .into_owned()
}

fn resolve_against(config_file: &Path, path: PathBuf) -> PathBuf {
    let path = expand_tilde(path);
    if path.is_absolute() {
        return path;
    }
    config_file
        .parent()
        .map_or(path.clone(), |dir| dir.join(&path))
}

/// The `target` the config at `path` names, if it does. Read on its own, before
/// the whole config is loaded, because the target is chosen before a command
/// loads the config. A file that is missing or is not valid TOML names none: the
/// command that loads the config reports that. A value that is not a string is an
/// error here; whether it names a target is for the command layer, which knows them.
pub fn target_setting(path: &Path) -> Result<Option<String>> {
    let Some(table) = peek(path) else {
        return Ok(None);
    };
    table
        .get("target")
        .map(|v| v.clone().try_into::<String>())
        .transpose()
        .with_context(|| format!("invalid `target` in {}", path.display()))
}

/// The `[targets.<name>]` table of the config at `path`, read on its own like
/// [`target_setting`] (the target is built before a command loads the config). A
/// table that is missing, or a file that is missing or not TOML, gives an empty
/// table; one that is not a table is an error. What is *in* it is for the target
/// to check.
pub fn target_settings(path: &Path, name: &str) -> Result<toml::Table> {
    let Some(file) = peek(path) else {
        return Ok(toml::Table::new());
    };
    file.get("targets")
        .and_then(|t| t.get(name))
        .map(|v| v.clone().try_into::<toml::Table>())
        .transpose()
        .map(Option::unwrap_or_default)
        .with_context(|| format!("invalid [targets.{name}] in {}", path.display()))
}

/// The config file as a raw table, `None` if it is missing or not TOML.
fn peek(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path).ok()?.parse().ok()
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

    /// Parses `raw` as if read from `path` (which relative paths resolve
    /// against), and checks it. What the file's syntax and types get wrong is
    /// reported on its own; everything else is reported together.
    pub fn parse(raw: &str, path: &Path) -> Result<Self> {
        let invalid = || format!("invalid config {}", path.display());
        let file: Config<RawSecret> = toml::from_str(raw).map_err(|e| {
            anyhow::anyhow!("{}{}", e.message(), location(raw, &e)).context(invalid())
        })?;
        let mut problems = Vec::new();
        let cfg = Self::from_file(file, path, &mut problems);
        problems.extend(cfg.problems());
        let report = match problems.len() {
            0 => return Ok(cfg),
            1 => problems.swap_remove(0),
            _ => format!("\n  - {}", problems.join("\n  - ")),
        };
        Err(anyhow::anyhow!(report).context(invalid()))
    }

    /// The config as read from `path`, with its host paths resolved and each
    /// `[[secret]]` checked; what is wrong with a secret goes to `problems`.
    fn from_file(file: Config<RawSecret>, path: &Path, problems: &mut Vec<String>) -> Self {
        let Config {
            target,
            targets,
            bundle,
            mut inject,
            session,
            git,
            secret,
        } = file;
        for item in &mut inject {
            item.binary = item.binary.take().map(expand_tilde);
            item.config = item.config.take().map(expand_tilde);
        }
        let secret = secret
            .into_iter()
            .filter_map(|raw| SecretDecl::try_from(raw).map_err(|e| problems.push(e)).ok())
            .map(|mut decl| {
                if let secret::Source::File(p) = &mut decl.from {
                    *p = expand_tilde(std::mem::take(p));
                }
                decl
            })
            .collect();
        Self {
            target,
            targets,
            bundle: bundle.map(|b| resolve_reference(path, b)),
            inject,
            session,
            git,
            secret,
        }
    }

    /// What the types can't check: every problem, so they are reported at once.
    fn problems(&self) -> Vec<String> {
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
        problems.extend(secret::problems(&self.secret));
        problems
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
        assert_eq!(c.git.safe_directories, vec!["*".to_string()]);
    }

    #[test]
    fn parses_full_config() {
        let c = parse(
            r#"
            [[inject]]
            name = "nvim"
            binary = "/usr/bin/nvim"

            [session]
            shell = "/bin/zsh"

            [git]
            safe_directories = ["/work"]
        "#,
        )
        .unwrap();
        assert_eq!(c.inject.len(), 1);
        assert_eq!(c.inject[0].name, "nvim");
        assert!(c.inject[0].copy_deps); // defaults to true
        assert_eq!(c.session.shell, "/bin/zsh");
        assert_eq!(c.git.safe_directories, vec!["/work".to_string()]);
    }

    #[test]
    fn the_target_is_a_name_the_command_layer_checks() {
        assert_eq!(
            parse("target = \"docker\"").unwrap().target.as_deref(),
            Some("docker")
        );
        assert_eq!(parse("").unwrap().target, None);
        // Which names are targets is for the command layer, which knows them; the
        // config only needs it to be a name.
        assert_eq!(
            parse("target = \"nonesuch\"").unwrap().target.as_deref(),
            Some("nonesuch")
        );
        assert!(parse("target = 3").is_err());
    }

    #[test]
    fn each_target_has_its_own_table_whose_keys_its_target_checks() {
        let c = parse(
            "[targets.docker]\ncontext = \"colima\"\n[targets.podman]\nconnection = \"vm\"\n",
        )
        .unwrap();
        assert_eq!(c.targets["docker"]["context"].as_str(), Some("colima"));
        assert_eq!(c.targets["podman"]["connection"].as_str(), Some("vm"));
        assert!(parse("").unwrap().targets.is_empty());
        // Which tables exist is for the command layer, which knows the targets; a
        // table that is not a table is the config's own error.
        assert!(parse("[targets]\ndocker = 3\n").is_err());
    }

    #[test]
    fn a_targets_table_can_be_read_alone_like_the_target() {
        let dir = crate::tmpfile::TempDir::new("target-settings").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "bogus = 1\n[targets.docker]\ncontext = \"colima\"\n").unwrap();
        let docker = target_settings(&path, "docker").unwrap();
        assert_eq!(docker["context"].as_str(), Some("colima"));
        // Another target's table, a missing table, a missing file, and a file that is
        // not TOML (the config load reports it) all give an empty table.
        assert!(target_settings(&path, "podman").unwrap().is_empty());
        assert!(
            target_settings(&dir.path().join("none.toml"), "docker")
                .unwrap()
                .is_empty()
        );
        std::fs::write(&path, "= not toml").unwrap();
        assert!(target_settings(&path, "docker").unwrap().is_empty());
        std::fs::write(&path, "[targets]\ndocker = 3\n").unwrap();
        let err = target_settings(&path, "docker").unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid [targets.docker]"),
            "{err:#}"
        );
    }

    #[test]
    fn the_target_alone_can_be_read_from_a_file_that_is_otherwise_unchecked() {
        let dir = crate::tmpfile::TempDir::new("target-peek").unwrap();
        let path = dir.path().join("c.toml");
        // Wrong in other ways (an unknown key): not this function's business.
        std::fs::write(&path, "target = \"docker\"\nbogus = 1\n").unwrap();
        assert_eq!(target_setting(&path).unwrap().as_deref(), Some("docker"));
        // Not TOML at all: no target named; loading the config reports it.
        std::fs::write(&path, "= = =").unwrap();
        assert_eq!(target_setting(&path).unwrap(), None);
        assert_eq!(target_setting(&dir.path().join("none.toml")).unwrap(), None);
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
            "/srv/lodge/b.tar"
        );
    }

    #[test]
    fn a_bundle_with_a_scheme_is_not_a_path_and_is_left_alone() {
        assert_eq!(
            parse("bundle = \"oci://ghcr.io/me/lodge:1\"")
                .unwrap()
                .bundle
                .unwrap(),
            "oci://ghcr.io/me/lodge:1"
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
            parse("[[inject]]\nname = \"x\"\nbinary = \"/x\"\ncopy_dep = true\n").unwrap_err()
        );
        assert!(err.contains("unknown field `copy_dep`"), "{err}");
        assert!(err.contains("line 4"), "{err}");
        assert!(err.contains("did you mean `copy_deps`?"), "{err}");

        let err = format!(
            "{:#}",
            parse("[sesion]\nshell = \"/bin/sh\"\n").unwrap_err()
        );
        assert!(err.contains("did you mean `session`?"), "{err}");
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
                 [[secret]]\nname = \"S\"\nfrom = { env = \"A\", file = \"/b\" }\nas = \"env\"\n\
                 [[secret]]\nname = \"T\"\nfrom = { env = \"A\" }\nas = \"socket\"\n"
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
        assert!(
            err.contains("secret \"S\": `from` needs exactly one"),
            "{err}"
        );
        assert!(err.contains("secret \"T\": unknown `as`"), "{err}");
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
