//! The client's own file, `client.toml`, beside `config.toml`: what depends on the
//! machine lodge runs on or on the person using it. Which target to work in and
//! how it is reached, where a bundle is read from, which secrets to deliver and
//! from where, the keys of the screens.
//!
//! None of it describes what is injected, so none of it is in a bundle: that is
//! a matter of which file it is in, not of a convention. Loaded and checked the
//! way `config.toml` is, all problems at once.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use super::secret::{self, RawSecret, SecretDecl};
use super::{ClientFile, expand_tilde, location, peek, report, resolve_reference};
use crate::files::Files;
use crate::keys::{KeyList, Keymap};

/// The file's name, in the folder of the config.
pub const FILE: &str = "client.toml";

// The client's settings. `S` is how a `[[secret]]` entry is held: as written
// (`RawSecret`, what the file is parsed into and the schema is made from), or
// checked (`SecretDecl`, the default, what the rest of lodge works with). A
// plain comment, not a doc comment: doc comments become the schema's text.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "S: Deserialize<'de>"))]
#[cfg_attr(
    test,
    derive(serde::Serialize, schemars::JsonSchema),
    schemars(rename = "Client")
)]
pub struct Client<S = SecretDecl> {
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
    /// entries of config.toml: a file, or a registry as `oci://ghcr.io/me/lodge:1`.
    /// A relative file path is relative to this file's folder.
    #[serde(default)]
    pub bundle: Option<String>,
    /// Other bundles to choose from, as `bundle` is written. `ldg up` offers them
    /// (with `bundle`, or the packed config when that is not set) when it injects
    /// into a project it has not opened before. `ldg pack -o` adds what it writes.
    #[serde(default)]
    pub bundles: Vec<String>,
    /// Secrets to deliver at attach time, and where each comes from on this machine.
    #[serde(default)]
    // Generic, so schemars can't work the default out itself.
    #[cfg_attr(test, schemars(extend("default" = [])))]
    pub secret: Vec<S>,
    /// The keys of the interactive screens, by what they do: `up = ["up", "k"]`.
    /// An action given here has only the keys given; one left out keeps its own.
    #[serde(default)]
    pub keys: BTreeMap<String, KeyList>,
}

impl<S> Default for Client<S> {
    fn default() -> Self {
        Self {
            target: None,
            targets: BTreeMap::new(),
            bundle: None,
            bundles: Vec::new(),
            secret: Vec::new(),
            keys: BTreeMap::new(),
        }
    }
}

impl Client {
    /// Parses `raw` as if read from `path` (which relative paths resolve
    /// against), and checks it. What the file's syntax and types get wrong is
    /// reported on its own; everything else is reported together.
    pub fn parse(raw: &str, path: &Path) -> Result<Self> {
        Self::read(raw, path).map_err(|problems| report(path, "", problems))
    }

    /// How many things are wrong with `raw`: none for a file that is valid.
    /// A file the types reject is one problem, the first they find.
    pub fn problem_count(raw: &str, path: &Path) -> usize {
        Self::read(raw, path)
            .err()
            .map_or(0, |problems| problems.len())
    }

    /// The settings in `raw`, or every problem with them.
    fn read(raw: &str, path: &Path) -> std::result::Result<Self, Vec<String>> {
        let file: Client<RawSecret> = toml::from_str(raw)
            .map_err(|e| vec![format!("{}{}", e.message(), location(raw, &e))])?;
        let mut problems = Vec::new();
        let client = Self::from_file(file, path, &mut problems);
        problems.extend(secret::problems(&client.secret));
        problems.extend(Keymap::build(&client.keys).err().unwrap_or_default());
        if problems.is_empty() {
            Ok(client)
        } else {
            Err(problems)
        }
    }

    /// The settings as read from `path`, with its host paths resolved and each
    /// `[[secret]]` checked; what is wrong with a secret goes to `problems`.
    fn from_file(file: Client<RawSecret>, path: &Path, problems: &mut Vec<String>) -> Self {
        let Client {
            target,
            targets,
            bundle,
            bundles,
            secret,
            keys,
        } = file;
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
            secret,
            keys,
        }
    }
}

/// The `target` the client file at `path` names, if it does. Read on its own, before
/// the whole file is loaded, because the target is chosen before a command
/// loads it. A file that is missing or is not valid TOML names none: the
/// command that loads the file reports that. A value that is not a string is an
/// error here; whether it names a target is for the command layer, which knows them.
pub fn target_setting(file: &ClientFile) -> Result<Option<String>> {
    let Some(table) = peek(file) else {
        return Ok(None);
    };
    table
        .get("target")
        .map(|v| v.clone().try_into::<String>())
        .transpose()
        .with_context(|| format!("invalid `target` in {}", file.display()))
}

/// The `[targets.<name>]` table of the client file at `path`, read on its own like
/// [`target_setting`] (the target is built before a command loads the file). A
/// table that is missing, or a file that is missing or not TOML, gives an empty
/// table; one that is not a table is an error. What is *in* it is for the target
/// to check.
pub fn target_settings(file: &ClientFile, name: &str) -> Result<toml::Table> {
    let Some(table) = peek(file) else {
        return Ok(toml::Table::new());
    };
    table
        .get("targets")
        .and_then(|t| t.get(name))
        .map(|v| v.clone().try_into::<toml::Table>())
        .transpose()
        .map(Option::unwrap_or_default)
        .with_context(|| format!("invalid [targets.{name}] in {}", file.display()))
}

/// The keys of the interactive screens, from the client file at `path` and read on
/// its own like [`target_setting`]: the first screen comes before any command
/// loads the file. A file that is missing, or not TOML, gives the defaults (the
/// command that loads the file reports the latter); a `[keys]` that is wrong
/// is an error, with everything that is wrong with it.
pub fn keymap(file: &ClientFile) -> Result<Keymap> {
    let Some(table) = peek(file) else {
        return Ok(Keymap::default());
    };
    let Some(table) = table.get("keys") else {
        return Ok(Keymap::default());
    };
    let settings: BTreeMap<String, KeyList> = table
        .clone()
        .try_into()
        .with_context(|| format!("invalid [keys] in {}", file.display()))?;
    Keymap::build(&settings).map_err(|problems| {
        anyhow::anyhow!("\n  - {}", problems.join("\n  - "))
            .context(format!("invalid [keys] in {}", file.display()))
    })
}

/// The file in a project's folder that says what is particular to the project:
/// only `[targets.<name>]` tables, for the targets that take settings per project.
pub const PROJECT_FILE: &str = "lodge.toml";

/// The `[targets.<name>]` table of the project file in `dir`: an empty table when
/// there is no file or no such table. The file may hold nothing but `targets`, so
/// a stray key (a `[[binary]]` that would be ignored) is an error and not a
/// silent no-op; what is *in* the table is for the target to check.
pub fn project_target_settings(files: &dyn Files, dir: &Path, name: &str) -> Result<toml::Table> {
    let path = dir.join(PROJECT_FILE);
    let Some(raw) = files
        .read(&path)
        .with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(toml::Table::new());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Document, Paths, absolute_reference, resolve_against};
    use crate::files::MemoryFiles;

    /// A client file holding `text`, kept in memory (none when `text` is `None`).
    fn client_with(text: Option<&str>) -> ClientFile {
        let files = MemoryFiles::new();
        let files = match text {
            Some(text) => files.with("/c/client.toml", text),
            None => files,
        };
        ClientFile::new("/c/client.toml", files)
    }

    fn parse(toml: &str) -> Result<Client> {
        Client::parse(toml, Path::new("/srv/lodge/client.toml"))
    }

    #[test]
    fn the_client_file_is_beside_the_config() {
        assert_eq!(
            Paths::beside(
                "/home/me/.config/lodge/config.toml".into(),
                MemoryFiles::new()
            )
            .client
            .path(),
            Path::new("/home/me/.config/lodge/client.toml")
        );
        // Whatever `-c` names, the client's file is in its folder.
        assert_eq!(
            Paths::beside("/srv/deploy/other.toml".into(), MemoryFiles::new())
                .client
                .path(),
            Path::new("/srv/deploy/client.toml")
        );
    }

    #[test]
    fn a_key_of_the_other_file_is_an_unknown_key() {
        let e = format!(
            "{:#}",
            parse("[[binary]]\nname = \"x\"\npath = \"/x\"\n").unwrap_err()
        );
        assert!(e.contains("unknown field `binary`"), "{e}");
        let e = format!(
            "{:#}",
            crate::config::Config::parse("target = \"docker\"\n", Path::new("/x/config.toml"))
                .unwrap_err()
        );
        assert!(e.contains("unknown field `target`"), "{e}");
    }

    #[test]
    fn an_empty_file_uses_defaults() {
        let c = parse("").unwrap();
        assert!(c.target.is_none() && c.bundle.is_none());
        assert!(c.bundles.is_empty() && c.secret.is_empty() && c.targets.is_empty());
    }

    #[test]
    fn a_secret_that_is_wrong_is_reported_with_the_rest() {
        let err = format!(
            "{:#}",
            parse(
                "[[secret]]\nname = \"S\"\nfrom = { env = \"A\", file = \"/b\" }\nas = \"env\"\n\
                 [[secret]]\nname = \"T\"\nfrom = { env = \"A\" }\nas = \"socket\"\n\
                 [keys]\nup = [\"bogus\"]\n"
            )
            .unwrap_err()
        );
        assert!(
            err.contains("secret \"S\": `from` needs exactly one"),
            "{err}"
        );
        assert!(err.contains("secret \"T\": unknown `as`"), "{err}");
        assert!(err.contains("`up`: `bogus` is not a key"), "{err}");
    }

    #[test]
    fn a_secret_file_expands_the_tilde() {
        let c =
            parse("[[secret]]\nname = \"T\"\nfrom = { file = \"~/tok\" }\nas = \"env\"\n").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(c.secret[0].from, secret::Source::File(home.join("tok")));
    }

    #[test]
    fn keys_are_checked_with_the_rest_of_the_config() {
        assert!(parse("[keys]\nleave = [\"esc\"]\npack = \"ctrl-k\"\n").is_ok());
        let e = format!(
            "{:#}",
            parse("[keys]\nlieve = [\"q\"]\nup = [\"bogus\"]\n[[secret]]\nname = \"a b\"\nfrom = { env = \"A\" }\nas = \"socket\"\n")
                .unwrap_err()
        );
        assert!(e.contains("`lieve` is not something a key does"), "{e}");
        assert!(e.contains("`up`: `bogus` is not a key"), "{e}");
        assert!(e.contains("invalid name \"a b\""), "all together: {e}");
    }

    #[test]
    fn the_keymap_is_read_on_its_own_and_a_missing_file_gives_the_defaults() {
        use crate::keys::{Action, Context};
        let q = "q".parse().unwrap();
        assert_eq!(
            keymap(&client_with(None)).unwrap().action(Context::List, q),
            Some(Action::Leave)
        );
        let file = client_with(Some("[keys]\nleave = [\"esc\"]\n"));
        assert_eq!(keymap(&file).unwrap().action(Context::List, q), None);
        let file = client_with(Some("[keys]\nup = [\"nope\"]\n"));
        let e = format!("{:#}", keymap(&file).unwrap_err());
        assert!(e.contains("invalid [keys]") && e.contains("`up`"), "{e}");
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
        let file = client_with(Some("bogus = 1\n[targets.docker]\ncontext = \"colima\"\n"));
        let docker = target_settings(&file, "docker").unwrap();
        assert_eq!(docker["context"].as_str(), Some("colima"));
        // Another target's table, a missing table, a missing file, and a file that is
        // not TOML (the config load reports it) all give an empty table.
        assert!(target_settings(&file, "podman").unwrap().is_empty());
        assert!(
            target_settings(&client_with(None), "docker")
                .unwrap()
                .is_empty()
        );
        let file = client_with(Some("= not toml"));
        assert!(target_settings(&file, "docker").unwrap().is_empty());
        let file = client_with(Some("[targets]\ndocker = 3\n"));
        let err = target_settings(&file, "docker").unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid [targets.docker]"),
            "{err:#}"
        );
    }

    #[test]
    fn the_target_alone_can_be_read_from_a_file_that_is_otherwise_unchecked() {
        // Wrong in other ways (an unknown key): not this function's business.
        let file = client_with(Some("target = \"docker\"\nbogus = 1\n"));
        assert_eq!(target_setting(&file).unwrap().as_deref(), Some("docker"));
        // Not TOML at all: no target named; loading the config reports it.
        let file = client_with(Some("= = ="));
        assert_eq!(target_setting(&file).unwrap(), None);
        assert_eq!(target_setting(&client_with(None)).unwrap(), None);
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
            Path::new("/srv/lodge").join("b.tar").to_string_lossy()
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
        let b = Path::new("/srv/lodge").join("b.tar");
        assert_eq!(
            c.bundles,
            [&*b.to_string_lossy(), "oci://ghcr.io/me/lodge:2"]
        );
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
    fn a_project_file_gives_a_targets_table_and_nothing_else() {
        let dir = Path::new("/p");
        let ask = |files: &MemoryFiles, name: &str| project_target_settings(files, dir, name);
        // No file: nothing.
        assert!(ask(&MemoryFiles::new(), "kubernetes").unwrap().is_empty());
        let with = |text: &str| MemoryFiles::new().with("/p/lodge.toml", text);
        let files =
            with("[targets.kubernetes]\npod = \"dev-0\"\n[targets.docker]\ncontext = \"x\"\n");
        let table = ask(&files, "kubernetes").unwrap();
        assert_eq!(table["pod"].as_str(), Some("dev-0"));
        assert!(ask(&files, "podman").unwrap().is_empty());
        // A key that would be ignored is refused, and so is a file that is not TOML.
        let err = ask(&with("[[binary]]\nname = \"x\"\n"), "kubernetes").unwrap_err();
        assert!(err.to_string().contains("`binary` is not allowed"), "{err}");
        assert!(ask(&with("= not toml"), "kubernetes").is_err());
    }
}
