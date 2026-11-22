//! The user's `config.toml`: what to inject, how to open sessions, which
//! secrets to deliver. Loaded once per run and validated as a whole, so every
//! mistake in it is reported together, before anything starts.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub mod edit;
pub mod secret;
pub mod unknown;

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
    /// A bundle to inject from instead of packing the `[[binary]]` and `[[data]]`
    /// entries: a file, or a registry as `oci://ghcr.io/me/lodge:1`. A relative
    /// file path is relative to this file's folder.
    #[serde(default)]
    pub bundle: Option<String>,
    /// Other bundles to choose from, as `bundle` is written. `ldg up` offers them
    /// (with `bundle`, or the packed config when that is not set) when it injects
    /// into a project it has not opened before. `ldg pack -o` adds what it writes.
    #[serde(default)]
    pub bundles: Vec<String>,
    /// Programs to inject, each with the shared libraries it needs.
    #[serde(default, rename = "binary")]
    pub binaries: Vec<BinaryEntry>,
    /// Files and directories to inject, as they are.
    #[serde(default)]
    pub data: Vec<DataEntry>,
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
            bundles: Vec::new(),
            binaries: Vec::new(),
            data: Vec::new(),
            session: SessionConfig::default(),
            git: GitConfig::default(),
            secret: Vec::new(),
        }
    }
}

/// A program to inject: the binary, relinked, with its libraries.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct BinaryEntry {
    /// Identifier, unique among the binaries; the binary's name in the bundle.
    pub name: String,
    /// Host path to the binary.
    pub path: PathBuf,
    /// Where to put an extra wrapper script that runs the binary (default: none:
    /// the binary is on the `PATH` of the sessions lodge opens).
    pub target: Option<String>,
    /// Pack the shared libraries `ldd` finds for the binary (default: true).
    #[serde(default = "default_true")]
    pub copy_deps: bool,
}

/// A file or directory to inject, copied as it is: a program's runtime files, a
/// config, a plugin folder. It is identified by where it goes, so no two entries
/// may go to the same place.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DataEntry {
    /// Host path to the file or directory.
    pub path: PathBuf,
    /// Where it goes in the container (default: the same place as here, `~/…`
    /// when it is under the home directory).
    pub target: Option<String>,
    /// The `name` of the `[[binary]]` this belongs to. Only says what it is for:
    /// it is injected the same either way.
    #[serde(rename = "for")]
    pub binary: Option<String>,
}

impl DataEntry {
    /// Where it goes in the container, given the home directory it is under here.
    pub fn dest(&self, home: &Path) -> String {
        match &self.target {
            Some(target) => target.clone(),
            None => crate::layout::tilde(&self.path, home),
        }
    }
}

/// The home directory, or `/` where there is none (and so no place is relative to it).
pub(crate) fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
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

/// A bundle reference for writing into the config from somewhere else than the
/// file's folder: a path made absolute (against the folder lodge runs in), so it
/// means the same wherever the config is read from; a `~/…` path or a reference
/// with a scheme is kept as it is.
pub fn absolute_reference(reference: &str) -> String {
    if reference.contains("://") || reference.starts_with('~') {
        return reference.to_string();
    }
    std::path::absolute(reference).map_or_else(
        |_| reference.to_string(),
        |p| p.to_string_lossy().into_owned(),
    )
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

/// The file in a project's folder that says what is particular to the project:
/// only `[targets.<name>]` tables, for the targets that take settings per project.
pub const PROJECT_FILE: &str = "lodge.toml";

/// The `[targets.<name>]` table of the project file in `dir`: an empty table when
/// there is no file or no such table. The file may hold nothing but `targets`, so
/// a stray key (a `[[binary]]` that would be ignored) is an error and not a
/// silent no-op; what is *in* the table is for the target to check.
pub fn project_target_settings(dir: &Path, name: &str) -> Result<toml::Table> {
    let path = dir.join(PROJECT_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: toml::Table = raw
        .parse()
        .with_context(|| format!("invalid {}", path.display()))?;
    if let Some(key) = file.keys().find(|k| *k != "targets") {
        anyhow::bail!(
            "invalid {}: `{key}` is not allowed; a project file holds only [targets.<name>] \
             tables (everything else is in your own config)",
            path.display()
        );
    }
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
        Self::read(raw, path).map_err(|mut problems| {
            let report = match problems.len() {
                1 => problems.swap_remove(0),
                _ => format!("\n  - {}", problems.join("\n  - ")),
            };
            anyhow::anyhow!(report).context(format!("invalid config {}", path.display()))
        })
    }

    /// How many things are wrong with `raw`: none for a config that is valid.
    /// A file the types reject is one problem, the first they find.
    pub fn problem_count(raw: &str, path: &Path) -> usize {
        Self::read(raw, path)
            .err()
            .map_or(0, |problems| problems.len())
    }

    /// The config in `raw`, or every problem with it.
    fn read(raw: &str, path: &Path) -> std::result::Result<Self, Vec<String>> {
        let file: Config<RawSecret> = toml::from_str(raw)
            .map_err(|e| vec![format!("{}{}", e.message(), location(raw, &e))])?;
        let mut problems = Vec::new();
        let cfg = Self::from_file(file, path, &mut problems);
        problems.extend(cfg.problems());
        if problems.is_empty() {
            Ok(cfg)
        } else {
            Err(problems)
        }
    }

    /// The config as read from `path`, with its host paths resolved and each
    /// `[[secret]]` checked; what is wrong with a secret goes to `problems`.
    fn from_file(file: Config<RawSecret>, path: &Path, problems: &mut Vec<String>) -> Self {
        let Config {
            target,
            targets,
            bundle,
            bundles,
            mut binaries,
            mut data,
            session,
            git,
            secret,
        } = file;
        for item in &mut binaries {
            item.path = expand_tilde(std::mem::take(&mut item.path));
        }
        for item in &mut data {
            item.path = expand_tilde(std::mem::take(&mut item.path));
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
            bundles: bundles
                .into_iter()
                .map(|b| resolve_reference(path, b))
                .collect(),
            binaries,
            data,
            session,
            git,
            secret,
        }
    }

    /// What the types can't check: every problem, so they are reported at once.
    fn problems(&self) -> Vec<String> {
        let mut problems = self.binary_problems();
        problems.extend(self.data_problems(&home_dir()));
        problems.extend(secret::problems(&self.secret));
        problems
    }

    fn binary_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen = HashSet::new();
        for item in &self.binaries {
            let name = &item.name;
            if name.trim().is_empty() {
                problems.push("a [[binary]] entry has an empty `name`".to_string());
            } else if !is_plain_name(name) {
                problems.push(format!(
                    "[[binary]] `{name}` is not usable as a name (letters, digits and `-_.+` only)"
                ));
            } else if !seen.insert(name.as_str()) {
                problems.push(format!(
                    "[[binary]] `{name}` is listed more than once (names must be unique)"
                ));
            }
            if let Some(why) = item.target.as_deref().and_then(container_path_problem) {
                problems.push(format!("[[binary]] `{name}`: `target` {why}"));
            }
        }
        problems
    }

    fn data_problems(&self, home: &Path) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen = HashSet::new();
        for item in &self.data {
            let dest = item.dest(home);
            let at = item.path.display();
            if let Some(why) = container_path_problem(&dest) {
                problems.push(format!(
                    "[[data]] `{at}`: the place it goes to, {dest}, {why}"
                ));
            } else if !seen.insert(dest.clone()) {
                problems.push(format!(
                    "[[data]] `{at}` goes to {dest}, where another [[data]] goes too"
                ));
            }
            if let Some(name) = &item.binary
                && !self.binaries.iter().any(|b| &b.name == name)
            {
                problems.push(format!(
                    "[[data]] `{at}`: `for` names `{name}`, which is not a [[binary]]"
                ));
            }
        }
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
        assert!(c.binaries.is_empty() && c.data.is_empty());
        assert_eq!(c.session.shell, "/bin/bash");
        assert_eq!(c.git.safe_directories, vec!["*".to_string()]);
    }

    #[test]
    fn parses_full_config() {
        let c = parse(
            r#"
            [[binary]]
            name = "nvim"
            path = "/usr/bin/nvim"

            [[data]]
            path = "/usr/local/share/nvim"
            for = "nvim"

            [[data]]
            path = "/home/me/.config/nvim"
            target = "~/.config/nvim"

            [session]
            shell = "/bin/zsh"

            [git]
            safe_directories = ["/work"]
        "#,
        )
        .unwrap();
        assert_eq!(c.binaries.len(), 1);
        assert_eq!(c.binaries[0].name, "nvim");
        assert!(c.binaries[0].copy_deps); // defaults to true
        assert_eq!(c.data.len(), 2);
        assert_eq!(c.data[0].binary.as_deref(), Some("nvim"));
        assert_eq!(c.data[0].target, None);
        assert_eq!(c.data[1].binary, None);
        assert_eq!(c.data[1].target.as_deref(), Some("~/.config/nvim"));
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
    fn the_listed_bundles_are_read_like_the_default_one() {
        let c = parse("bundles = [\"b.tar\", \"oci://ghcr.io/me/lodge:2\"]").unwrap();
        assert_eq!(c.bundles, ["/srv/lodge/b.tar", "oci://ghcr.io/me/lodge:2"]);
        assert!(parse("").unwrap().bundles.is_empty());
    }

    #[test]
    fn a_reference_written_into_the_config_keeps_its_scheme_and_its_tilde() {
        assert_eq!(
            absolute_reference("oci://ghcr.io/me/lodge:1"),
            "oci://ghcr.io/me/lodge:1"
        );
        assert_eq!(absolute_reference("~/x.tar"), "~/x.tar");
        assert!(Path::new(&absolute_reference("x.tar")).is_absolute());
        assert!(absolute_reference("x.tar").ends_with("x.tar"));
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
            "[[binary]]\nname = \"x\"\npath = \"~/bin/x\"\n\
             [[secret]]\nname = \"T\"\nfrom = { file = \"~/tok\" }\nas = \"env\"\n",
        )
        .unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(c.binaries[0].path.as_path(), home.join("bin/x").as_path());
        assert_eq!(c.secret[0].from, secret::Source::File(home.join("tok")));
    }

    #[test]
    fn data_goes_where_it_is_here_unless_it_says_otherwise() {
        let home = dirs::home_dir().unwrap();
        let c = parse(
            "[[data]]\npath = \"~/.config/nvim\"\n\
             [[data]]\npath = \"/usr/local/share/nvim\"\n\
             [[data]]\npath = \"/usr/local/lib/nvim\"\ntarget = \"~/lib/nvim\"\n",
        )
        .unwrap();
        assert_eq!(c.data[0].path, home.join(".config/nvim"));
        assert_eq!(c.data[0].dest(&home), "~/.config/nvim");
        assert_eq!(c.data[1].dest(&home), "/usr/local/share/nvim");
        assert_eq!(c.data[2].dest(&home), "~/lib/nvim");
        // Where it is under is the caller's to say: another home, another place.
        assert_eq!(
            c.data[0].dest(Path::new("/elsewhere")),
            home.join(".config/nvim").display().to_string()
        );
    }

    #[test]
    fn a_typo_is_an_error_with_its_line_and_a_suggestion() {
        let err = format!(
            "{:#}",
            parse("[[binary]]\nname = \"x\"\npath = \"/x\"\ncopy_dep = true\n").unwrap_err()
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
                "[[binary]]\nname = \"a\"\npath = \"/a\"\n\
                 [[binary]]\nname = \"a\"\npath = \"/b\"\n\
                 [[binary]]\nname = \"../up\"\npath = \"/u\"\n\
                 [[data]]\npath = \"/t\"\ntarget = \"rel/x\"\n\
                 [[data]]\npath = \"/m\"\ntarget = \"/same\"\n\
                 [[data]]\npath = \"/n\"\ntarget = \"/same\"\n\
                 [[data]]\npath = \"/o\"\nfor = \"nope\"\n\
                 [[secret]]\nname = \"S\"\nfrom = { env = \"A\", file = \"/b\" }\nas = \"env\"\n\
                 [[secret]]\nname = \"T\"\nfrom = { env = \"A\" }\nas = \"socket\"\n"
            )
            .unwrap_err()
        );
        assert!(err.contains("`a` is listed more than once"), "{err}");
        assert!(err.contains("`../up` is not usable as a name"), "{err}");
        assert!(
            err.contains("[[data]] `/t`: the place it goes to, rel/x, must be absolute"),
            "{err}"
        );
        assert!(
            err.contains("[[data]] `/n` goes to /same, where another [[data]] goes too"),
            "{err}"
        );
        assert!(
            err.contains("[[data]] `/o`: `for` names `nope`, which is not a [[binary]]"),
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

    #[test]
    fn a_project_file_gives_a_targets_table_and_nothing_else() {
        let dir = crate::tmpfile::TempDir::new("project-file").unwrap();
        // No file: nothing.
        assert!(
            project_target_settings(dir.path(), "kubernetes")
                .unwrap()
                .is_empty()
        );
        std::fs::write(
            dir.path().join(PROJECT_FILE),
            "[targets.kubernetes]\npod = \"dev-0\"\n[targets.docker]\ncontext = \"x\"\n",
        )
        .unwrap();
        let table = project_target_settings(dir.path(), "kubernetes").unwrap();
        assert_eq!(table["pod"].as_str(), Some("dev-0"));
        assert!(
            project_target_settings(dir.path(), "podman")
                .unwrap()
                .is_empty()
        );
        // A key that would be ignored is refused, and so is a file that is not TOML.
        std::fs::write(dir.path().join(PROJECT_FILE), "[[binary]]\nname = \"x\"\n").unwrap();
        let err = project_target_settings(dir.path(), "kubernetes").unwrap_err();
        assert!(err.to_string().contains("`binary` is not allowed"), "{err}");
        std::fs::write(dir.path().join(PROJECT_FILE), "= not toml").unwrap();
        assert!(project_target_settings(dir.path(), "kubernetes").is_err());
    }
}
