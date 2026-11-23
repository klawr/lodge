//! The user's `config.toml`: what to inject and how to open sessions. It is what
//! may travel with a bundle, so nothing in it depends on the machine lodge runs
//! on. What does (the target, where a bundle is read from, which secrets to
//! deliver and from where, the keys) is the [`client`]'s own file, `client.toml`.
//! Each is loaded once per run and validated as a whole, so every mistake in it
//! is reported together, before anything starts.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod client;
pub mod edit;
pub mod secret;
pub mod unknown;

use crate::files::Files;
use crate::shell::{container_path_problem, is_plain_name};

/// The user's config. What is in it is what a bundle is made from.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    test,
    derive(Serialize, schemars::JsonSchema),
    schemars(rename = "Config")
)]
pub struct Config {
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

/// The file's name, in the config folder.
pub const FILE: &str = "config.toml";

/// A file lodge reads and edits as one kind of document: it knows how to check a
/// text as itself, what its types do not know, and where it is kept ([`Files`]).
/// The edit functions take the kind they work on, so a setting can't be written
/// into the wrong file, and the parts that work on either (checking, finding
/// what is unknown) take this.
pub trait Document {
    fn path(&self) -> &Path;
    /// Where its text is read from and written to.
    fn files(&self) -> &dyn Files;
    /// Checks `raw` as this kind of file. An error is every problem with it.
    fn check(&self, raw: &str) -> Result<()>;
    /// How many things are wrong with `raw`: none for a text that is valid.
    fn problem_count(&self, raw: &str) -> usize;
    /// The error its types stop at, when `raw` holds a key they do not know.
    fn unknown_field(&self, raw: &str) -> Option<toml::de::Error>;

    /// What the file holds, `None` when it is not there.
    fn read_text(&self) -> Result<Option<String>> {
        self.files()
            .read(self.path())
            .with_context(|| format!("reading {}", self.path().display()))
    }

    /// Replaces what the file holds with `text`, which is not checked here.
    fn write_text(&self, text: &str) -> Result<()> {
        self.files()
            .write(self.path(), text)
            .with_context(|| format!("writing {}", self.path().display()))
    }

    fn exists(&self) -> bool {
        self.files().exists(self.path())
    }
}

/// The path of the config, `config.toml`: what is injected.
#[derive(Clone)]
pub struct ConfigFile(PathBuf, Arc<dyn Files>);

/// The path of the client's own file, `client.toml`.
#[derive(Clone)]
pub struct ClientFile(PathBuf, Arc<dyn Files>);

macro_rules! file_path {
    ($t:ident) => {
        impl $t {
            pub fn new(path: impl Into<PathBuf>, files: Arc<dyn Files>) -> Self {
                Self(path.into(), files)
            }
        }
        impl $t {
            /// For messages: the path.
            pub fn display(&self) -> std::path::Display<'_> {
                self.0.display()
            }
        }
        impl PartialEq for $t {
            fn eq(&self, other: &Self) -> bool {
                self.0 == other.0
            }
        }
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($t)).field(&self.0).finish()
            }
        }
    };
}
file_path!(ConfigFile);
file_path!(ClientFile);

impl ConfigFile {
    /// The files it is kept in, for another path to be kept in the same ones.
    pub fn files_handle(&self) -> Arc<dyn Files> {
        self.1.clone()
    }

    /// The config in this file. A missing file is an empty config: nothing is
    /// injected, and you still get a shell.
    pub fn load(&self) -> Result<Config> {
        match self.read_text()? {
            Some(raw) => Config::parse(&raw, &self.0),
            None => Ok(Config::default()),
        }
    }
}

impl ClientFile {
    /// The settings in this file. A missing file is an empty one: every setting
    /// has its default.
    pub fn load(&self) -> Result<client::Client> {
        match self.read_text()? {
            Some(raw) => client::Client::parse(&raw, &self.0),
            None => Ok(client::Client::default()),
        }
    }
}

impl Document for ConfigFile {
    fn path(&self) -> &Path {
        &self.0
    }
    fn files(&self) -> &dyn Files {
        self.1.as_ref()
    }
    fn check(&self, raw: &str) -> Result<()> {
        Config::parse(raw, &self.0).map(drop)
    }
    fn problem_count(&self, raw: &str) -> usize {
        Config::problem_count(raw)
    }
    fn unknown_field(&self, raw: &str) -> Option<toml::de::Error> {
        toml::from_str::<Config>(raw).err()
    }
}

impl Document for ClientFile {
    fn path(&self) -> &Path {
        &self.0
    }
    fn files(&self) -> &dyn Files {
        self.1.as_ref()
    }
    fn check(&self, raw: &str) -> Result<()> {
        client::Client::parse(raw, &self.0).map(drop)
    }
    fn problem_count(&self, raw: &str) -> usize {
        client::Client::problem_count(raw, &self.0)
    }
    fn unknown_field(&self, raw: &str) -> Option<toml::de::Error> {
        toml::from_str::<client::Client<secret::RawSecret>>(raw).err()
    }
}

/// Where the two files are for this run, and the files they are kept in. Made
/// once, in the composition root, and handed down: nothing derives one path from
/// the other.
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub config: ConfigFile,
    pub client: ClientFile,
}

impl Paths {
    /// The files for the config at `config`: the client's is `client.toml` in the
    /// same folder.
    pub fn beside(config: PathBuf, files: Arc<dyn Files>) -> Self {
        Self {
            client: ClientFile::new(config.with_file_name(client::FILE), files.clone()),
            config: ConfigFile::new(config, files),
        }
    }

    /// The files for this run: the config `-c` names, which must exist, or the
    /// default one (see [`path_for`]), and the client's beside it.
    pub fn for_run(flag: Option<PathBuf>, files: Arc<dyn Files>) -> Result<Self> {
        path_for(flag, files.as_ref()).map(|config| Self::beside(config, files))
    }

    /// Both, the config first.
    pub fn documents(&self) -> [&dyn Document; 2] {
        [&self.config, &self.client]
    }
}

/// Every problem as one error: the only one as it is, several as a list.
fn report(path: &Path, what: &str, mut problems: Vec<String>) -> anyhow::Error {
    let report = match problems.len() {
        1 => problems.swap_remove(0),
        _ => format!("\n  - {}", problems.join("\n  - ")),
    };
    anyhow::anyhow!(report).context(format!("invalid {what}{}", path.display()))
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
        .join(FILE))
}

/// The config file for this run: `-c FILE`, which must exist, or the default
/// location, which may not. Absolute, so a relative `bundle =` resolves the same
/// wherever lodge runs from.
pub fn path_for(flag: Option<PathBuf>, files: &dyn Files) -> Result<PathBuf> {
    let Some(path) = flag else {
        return default_path();
    };
    let path = expand_tilde(path);
    if !files.exists(&path) {
        anyhow::bail!("config file {} does not exist", path.display());
    }
    if path.file_name().is_some_and(|n| n == client::FILE) {
        anyhow::bail!(
            "{} is the client's file, which sits beside the config; give the config with -c",
            path.display()
        );
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

/// A file as a raw table, `None` if it is missing, unreadable or not TOML.
fn peek(file: &dyn Document) -> Option<toml::Table> {
    file.read_text().ok()??.parse().ok()
}

impl Config {
    /// Parses `raw` as if read from `path`, and checks it. What the file's syntax
    /// and types get wrong is reported on its own; everything else is reported
    /// together.
    pub fn parse(raw: &str, path: &Path) -> Result<Self> {
        Self::read(raw).map_err(|problems| report(path, "config ", problems))
    }

    /// How many things are wrong with `raw`: none for a config that is valid.
    /// A file the types reject is one problem, the first they find.
    pub fn problem_count(raw: &str) -> usize {
        Self::read(raw).err().map_or(0, |problems| problems.len())
    }

    /// The config in `raw`, or every problem with it.
    fn read(raw: &str) -> std::result::Result<Self, Vec<String>> {
        let mut cfg: Config = toml::from_str(raw)
            .map_err(|e| vec![format!("{}{}", e.message(), location(raw, &e))])?;
        for item in &mut cfg.binaries {
            item.path = expand_tilde(std::mem::take(&mut item.path));
        }
        for item in &mut cfg.data {
            item.path = expand_tilde(std::mem::take(&mut item.path));
        }
        let problems = cfg.problems();
        if problems.is_empty() {
            Ok(cfg)
        } else {
            Err(problems)
        }
    }

    /// What the types can't check: every problem, so they are reported at once.
    fn problems(&self) -> Vec<String> {
        let mut problems = self.binary_problems();
        problems.extend(self.data_problems(&home_dir()));
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

/// " (line N)" plus a "did you mean" for an unknown key that is close to a known
/// one, from a toml error.
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
    fn host_paths_expand_the_tilde() {
        let c = parse("[[binary]]\nname = \"x\"\npath = \"~/bin/x\"\n").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(c.binaries[0].path.as_path(), home.join("bin/x").as_path());
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
                 [[data]]\npath = \"/o\"\nfor = \"nope\"\n"
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
