//! Changing the `[[binary]]` and `[[data]]` entries of the config file from the
//! command line and the config screen.
//!
//! The file is edited as text (comments, order and layout stay as the person
//! wrote them), and what would be written is parsed and checked as a whole
//! before the file is touched, so an edit can never leave a config lodge
//! refuses to load.

use anyhow::{Context, Result, bail};
use std::path::Path;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use super::Config;
use super::unknown::{self, Unknown};

/// The two kinds of entry, each a table of its own name in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Binary,
    Data,
}

impl Kind {
    /// The name of the table: `[[binary]]`, `[[data]]`.
    pub fn key(self) -> &'static str {
        match self {
            Kind::Binary => "binary",
            Kind::Data => "data",
        }
    }

    /// The fields an entry of this kind has, in the order they are written.
    pub fn fields(self) -> &'static [Field] {
        match self {
            Kind::Binary => &[Field::Name, Field::Path, Field::Target, Field::CopyDeps],
            Kind::Data => &[Field::Path, Field::Target, Field::For],
        }
    }

    /// The field that tells entries of this kind apart: a program is known by
    /// its name, data by where it comes from.
    fn identity(self) -> Field {
        match self {
            Kind::Binary => Field::Name,
            Kind::Data => Field::Path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Path,
    Target,
    CopyDeps,
    For,
}

impl Field {
    pub fn key(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Path => "path",
            Field::Target => "target",
            Field::CopyDeps => "copy_deps",
            Field::For => "for",
        }
    }
}

/// What to add, paths as they are to be written: a program or data, each with
/// only what it has.
#[derive(Clone)]
pub enum NewEntry {
    Binary(NewBinary),
    Data(NewData),
}

#[derive(Clone)]
pub struct NewBinary {
    pub name: String,
    pub path: String,
    pub target: Option<String>,
    pub copy_deps: bool,
}

#[derive(Clone)]
pub struct NewData {
    pub path: String,
    pub target: Option<String>,
    pub binary: Option<String>,
}

impl NewEntry {
    /// A program with the defaults: no wrapper, its libraries packed.
    pub fn binary(name: &str, path: String) -> Self {
        NewEntry::Binary(NewBinary {
            name: name.into(),
            path,
            target: None,
            copy_deps: true,
        })
    }

    pub fn data(path: String, target: Option<String>, binary: Option<String>) -> Self {
        NewEntry::Data(NewData {
            path,
            target,
            binary,
        })
    }

    pub fn kind(&self) -> Kind {
        match self {
            NewEntry::Binary(_) => Kind::Binary,
            NewEntry::Data(_) => Kind::Data,
        }
    }

    /// What tells it apart from the others of its kind: a program's name, data's path.
    pub fn key(&self) -> &str {
        match self {
            NewEntry::Binary(b) => &b.name,
            NewEntry::Data(d) => &d.path,
        }
    }

    /// The host path, as it is to be written.
    pub fn path(&self) -> &str {
        match self {
            NewEntry::Binary(b) => &b.path,
            NewEntry::Data(d) => &d.path,
        }
    }
}

/// An entry as it is written in the file: nothing expanded, nothing defaulted, so
/// what is shown is what is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub kind: Kind,
    pub name: Option<String>,
    pub path: Option<String>,
    pub target: Option<String>,
    pub copy_deps: Option<bool>,
    pub binary: Option<String>,
}

impl RawEntry {
    /// What tells it apart from the others of its kind.
    pub fn key(&self) -> &str {
        match self.kind {
            Kind::Binary => self.name.as_deref().unwrap_or_default(),
            Kind::Data => self.path.as_deref().unwrap_or_default(),
        }
    }
}

/// Adds an entry to the config at `path` (created when missing).
pub fn add(path: &Path, entry: &NewEntry) -> Result<()> {
    let mut doc = read(path)?;
    let kind = entry.kind();
    let key = kind.key();
    if entries(&doc, kind).any(|t| identity_of(kind, t) == Some(entry.key())) {
        bail!(
            "a [[{key}]] `{}` is already in {}; remove it first to replace it",
            entry.key(),
            path.display(),
        );
    }
    let mut table = Table::new();
    match entry {
        NewEntry::Binary(b) => {
            table["name"] = value(&b.name);
            table["path"] = value(&b.path);
            if let Some(target) = &b.target {
                table["target"] = value(target.as_str());
            }
            if !b.copy_deps {
                table["copy_deps"] = value(false);
            }
        }
        NewEntry::Data(d) => {
            table["path"] = value(&d.path);
            if let Some(target) = &d.target {
                table["target"] = value(target.as_str());
            }
            if let Some(binary) = &d.binary {
                table["for"] = value(binary);
            }
        }
    }
    // A file of nothing but comments (what `ldg init` writes) keeps them above the
    // entry, where they were, and not after it.
    if doc.as_table().is_empty()
        && let Some(text) = doc.trailing().as_str().filter(|t| !t.is_empty())
    {
        let text = text.to_string();
        let sep = if text.ends_with("\n\n") { "" } else { "\n" };
        table.decor_mut().set_prefix(format!("{text}{sep}"));
        doc.set_trailing("");
    }
    let list = doc
        .entry(key)
        .or_insert_with(|| Item::ArrayOfTables(Default::default()))
        .as_array_of_tables_mut()
        .with_context(|| format!("`{key}` in the config is not a list of [[{key}]] tables"))?;
    list.push(table);
    write(path, &doc)
}

/// Removes the entry of `kind` known as `key` (see [`RawEntry::key`]). Data that
/// was for a removed binary stays, no longer for anything.
pub fn remove(path: &Path, kind: Kind, key: &str) -> Result<()> {
    let mut doc = read(path)?;
    let missing = || format!("no [[{}]] `{key}` in {}", kind.key(), path.display());
    let Some(list) = doc
        .get_mut(kind.key())
        .and_then(Item::as_array_of_tables_mut)
    else {
        bail!(missing());
    };
    let Some(at) = list.iter().position(|t| identity_of(kind, t) == Some(key)) else {
        bail!(missing());
    };
    list.remove(at);
    if list.is_empty() {
        doc.remove(kind.key());
    }
    if kind == Kind::Binary {
        point_data_at(&mut doc, key, None);
    }
    write(path, &doc)
}

/// The entries of the config at `path` of `kind`, as written (none when it is
/// missing).
pub fn list(path: &Path, kind: Kind) -> Result<Vec<RawEntry>> {
    let doc = read(path)?;
    let text = |t: &Table, key: &str| t.get(key).and_then(Item::as_str).map(String::from);
    Ok(entries(&doc, kind)
        .map(|t| RawEntry {
            kind,
            name: text(t, "name"),
            path: text(t, "path"),
            target: text(t, "target"),
            copy_deps: t.get("copy_deps").and_then(Item::as_bool),
            binary: text(t, "for"),
        })
        .collect())
}

/// What the config at `path` holds that this version of lodge does not know (none
/// when it is missing).
pub fn unknown(path: &Path) -> Result<Vec<Unknown>> {
    Ok(unknown::find(&read_text(path)?))
}

/// Takes `which` out of the config at `path`. It is the one edit that needs no
/// valid config as its result: what it takes out is what keeps one from loading.
pub fn remove_unknown(path: &Path, which: &Unknown) -> Result<()> {
    let text = unknown::remove(&read_text(path)?, which)
        .with_context(|| format!("`{}` is no longer in {}", which.label, path.display()))?;
    store(path, &text)
}

/// Sets a text field of the entry of `kind` known as `key`, or takes it out with
/// `None` (`copy_deps` is not text: see [`set_copy_deps`]). The result is checked as
/// a whole, so a name in use or a target the container can't use is refused. The
/// data that is for a binary follows it when the binary is renamed.
pub fn set_field(path: &Path, kind: Kind, key: &str, field: Field, to: Option<&str>) -> Result<()> {
    if !kind.fields().contains(&field) {
        bail!("a [[{}]] has no `{}`", kind.key(), field.key());
    }
    if field == Field::CopyDeps {
        bail!("copy_deps is on or off, not text");
    }
    if to.is_none() && (field == kind.identity() || field == Field::Path) {
        bail!("an entry needs a {}", field.key());
    }
    let mut doc = read(path)?;
    change_in(&mut doc, path, kind, key, |t| match to {
        Some(v) => t[field.key()] = value(v),
        None => {
            t.remove(field.key());
        }
    })?;
    if kind == Kind::Binary
        && field == Field::Name
        && let Some(new) = to
    {
        point_data_at(&mut doc, key, Some(new));
    }
    write(path, &doc)
}

/// Sets `copy_deps` of the binary called `name`; `true` is the default, so it is
/// taken out of the file instead of written.
pub fn set_copy_deps(path: &Path, name: &str, on: bool) -> Result<()> {
    let mut doc = read(path)?;
    change_in(&mut doc, path, Kind::Binary, name, |t| {
        if on {
            t.remove("copy_deps");
        } else {
            t["copy_deps"] = value(false);
        }
    })?;
    write(path, &doc)
}

fn change_in(
    doc: &mut DocumentMut,
    path: &Path,
    kind: Kind,
    key: &str,
    edit: impl FnOnce(&mut Table),
) -> Result<()> {
    let table = doc
        .get_mut(kind.key())
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|list| list.iter_mut().find(|t| identity_of(kind, t) == Some(key)))
        .with_context(|| format!("no [[{}]] `{key}` in {}", kind.key(), path.display()))?;
    edit(table);
    Ok(())
}

/// Makes the data that is for the binary `old` be for `new`, or for nothing.
fn point_data_at(doc: &mut DocumentMut, old: &str, new: Option<&str>) {
    let Some(list) = doc.get_mut("data").and_then(Item::as_array_of_tables_mut) else {
        return;
    };
    for table in list.iter_mut() {
        if table.get("for").and_then(Item::as_str) == Some(old) {
            match new {
                Some(name) => table["for"] = value(name),
                None => {
                    table.remove("for");
                }
            }
        }
    }
}

/// The single values of the file: `target` and `bundle` at the top, `shell` under
/// `[session]`, `safe_directories` under `[git]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Target,
    Bundle,
    Shell,
    SafeDirs,
}

impl Setting {
    pub const ALL: [Setting; 4] = [
        Setting::Target,
        Setting::Bundle,
        Setting::Shell,
        Setting::SafeDirs,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Setting::Target => "target",
            Setting::Bundle => "bundle",
            Setting::Shell => "shell",
            Setting::SafeDirs => "safe_directories",
        }
    }

    /// The table it is in; none for the top of the file.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Setting::Target | Setting::Bundle => None,
            Setting::Shell => Some("session"),
            Setting::SafeDirs => Some("git"),
        }
    }
}

/// A setting as it is written in the file (a quoted string, or a list like `["*"]`), none when it is
/// not there.
pub fn setting(path: &Path, which: Setting) -> Result<Option<String>> {
    let doc = read(path)?;
    let table = match which.section() {
        None => Some(doc.as_table()),
        Some(name) => doc.get(name).and_then(Item::as_table),
    };
    Ok(table
        .and_then(|t| t.get(which.key()))
        .map(|i| i.to_string().trim().to_string()))
}

/// Sets a setting, or takes it out (back to its default) with `None`.
/// `safe_directories` is a list: the text is split at commas, and `none` is an
/// empty list, which switches the setting off.
pub fn set_setting(path: &Path, which: Setting, to: Option<&str>) -> Result<()> {
    let mut doc = read(path)?;
    if which.section().is_some_and(|s| doc.get(s).is_none()) && to.is_some() {
        doc[which.section().unwrap_or_default()] = Item::Table(Table::new());
    }
    let table = match which.section() {
        None => Some(doc.as_table_mut()),
        Some(name) => doc.get_mut(name).and_then(Item::as_table_mut),
    };
    if let Some(table) = table {
        match to {
            None => {
                table.remove(which.key());
            }
            Some(text) if which == Setting::SafeDirs => {
                let mut list = Array::new();
                if text.trim() != "none" {
                    for dir in text.split(',').map(str::trim).filter(|d| !d.is_empty()) {
                        list.push(dir);
                    }
                }
                table[which.key()] = value(list);
            }
            Some(text) => table[which.key()] = value(text),
        }
        // A table left with nothing in it is not kept.
        if let Some(name) = which.section()
            && doc
                .get(name)
                .and_then(Item::as_table)
                .is_some_and(Table::is_empty)
        {
            doc.remove(name);
        }
    }
    write(path, &doc)
}

/// The bundles listed under `bundles`, as written.
pub fn bundles(path: &Path) -> Result<Vec<String>> {
    let doc = read(path)?;
    Ok(doc
        .get("bundles")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// Adds `reference` to `bundles`. `false` when it was listed already.
pub fn add_bundle(path: &Path, reference: &str) -> Result<bool> {
    let mut list = bundles(path)?;
    if list.iter().any(|b| b == reference) {
        return Ok(false);
    }
    list.push(reference.to_string());
    set_bundles(path, &list)?;
    Ok(true)
}

/// Takes `reference` out of `bundles`. `false` when it was not there.
pub fn remove_bundle(path: &Path, reference: &str) -> Result<bool> {
    let mut list = bundles(path)?;
    let before = list.len();
    list.retain(|b| b != reference);
    if list.len() == before {
        return Ok(false);
    }
    set_bundles(path, &list)?;
    Ok(true)
}

/// Writes the list, one reference to a line; no list is no key.
pub fn set_bundles(path: &Path, list: &[String]) -> Result<()> {
    let mut doc = read(path)?;
    if list.is_empty() {
        doc.remove("bundles");
    } else {
        let mut array = Array::new();
        for reference in list {
            array.push(reference.as_str());
        }
        array.set_trailing_comma(true);
        for item in array.iter_mut() {
            item.decor_mut().set_prefix("\n    ");
        }
        array.set_trailing("\n");
        doc["bundles"] = value(array);
    }
    write(path, &doc)
}

/// Where a secret's value comes from, on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretFrom {
    Env(String),
    File(String),
    Command(String),
}

impl SecretFrom {
    pub fn kind(&self) -> &'static str {
        match self {
            SecretFrom::Env(_) => "env",
            SecretFrom::File(_) => "file",
            SecretFrom::Command(_) => "command",
        }
    }

    fn item(&self) -> Item {
        let (kind, text) = match self {
            SecretFrom::Env(v) | SecretFrom::File(v) | SecretFrom::Command(v) => (self.kind(), v),
        };
        let mut t = InlineTable::new();
        t.insert(kind, text.as_str().into());
        Item::Value(Value::InlineTable(t))
    }
}

/// How a secret is presented in the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretAs {
    Env,
    File { path: String, mode: Option<String> },
}

impl SecretAs {
    fn item(&self) -> Item {
        match self {
            SecretAs::Env => value("env"),
            SecretAs::File { path, mode } => {
                let mut t = InlineTable::new();
                t.insert("file", path.as_str().into());
                if let Some(mode) = mode {
                    t.insert("mode", mode.as_str().into());
                }
                Item::Value(Value::InlineTable(t))
            }
        }
    }
}

/// What to add as a `[[secret]]`.
pub struct NewSecret {
    pub name: String,
    pub from: SecretFrom,
    pub as_: SecretAs,
}

/// A `[[secret]]` as written: each field as the text of its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSecret {
    pub name: String,
    pub from: Option<String>,
    pub as_: Option<String>,
    pub targets: Option<String>,
}

pub fn list_secrets(path: &Path) -> Result<Vec<RawSecret>> {
    let doc = read(path)?;
    let text = |t: &Table, key: &str| t.get(key).map(|i| i.to_string().trim().to_string());
    Ok(secrets(&doc)
        .map(|t| RawSecret {
            name: t
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string(),
            from: text(t, "from"),
            as_: text(t, "as"),
            targets: text(t, "targets"),
        })
        .collect())
}

pub fn add_secret(path: &Path, secret: &NewSecret) -> Result<()> {
    let mut doc = read(path)?;
    if secrets(&doc).any(|t| name_of(t) == Some(secret.name.as_str())) {
        bail!(
            "a [[secret]] `{}` is already in {}",
            secret.name,
            path.display()
        );
    }
    let mut table = Table::new();
    table["name"] = value(&secret.name);
    table["from"] = secret.from.item();
    table["as"] = secret.as_.item();
    let list = doc
        .entry("secret")
        .or_insert_with(|| Item::ArrayOfTables(Default::default()))
        .as_array_of_tables_mut()
        .context("`secret` in the config is not a list of [[secret]] tables")?;
    list.push(table);
    write(path, &doc)
}

pub fn remove_secret(path: &Path, name: &str) -> Result<()> {
    let mut doc = read(path)?;
    let missing = || format!("no [[secret]] `{name}` in {}", path.display());
    let Some(list) = doc.get_mut("secret").and_then(Item::as_array_of_tables_mut) else {
        bail!(missing());
    };
    let Some(at) = list.iter().position(|t| name_of(t) == Some(name)) else {
        bail!(missing());
    };
    list.remove(at);
    if list.is_empty() {
        doc.remove("secret");
    }
    write(path, &doc)
}

pub fn rename_secret(path: &Path, name: &str, to: &str) -> Result<()> {
    change_secret(path, name, |t| t["name"] = value(to))
}

pub fn set_secret_from(path: &Path, name: &str, from: &SecretFrom) -> Result<()> {
    change_secret(path, name, |t| t["from"] = from.item())
}

pub fn set_secret_as(path: &Path, name: &str, as_: &SecretAs) -> Result<()> {
    change_secret(path, name, |t| t["as"] = as_.item())
}

/// The hosts and container names a secret may go to; none means anywhere.
pub fn set_secret_targets(path: &Path, name: &str, targets: &[String]) -> Result<()> {
    change_secret(path, name, |t| {
        if targets.is_empty() {
            t.remove("targets");
        } else {
            let mut list = Array::new();
            for target in targets {
                list.push(target.as_str());
            }
            t["targets"] = value(list);
        }
    })
}

fn change_secret(path: &Path, name: &str, edit: impl FnOnce(&mut Table)) -> Result<()> {
    let mut doc = read(path)?;
    let table = doc
        .get_mut("secret")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|list| list.iter_mut().find(|t| name_of(t) == Some(name)))
        .with_context(|| format!("no [[secret]] `{name}` in {}", path.display()))?;
    edit(table);
    write(path, &doc)
}

fn secrets(doc: &DocumentMut) -> impl Iterator<Item = &Table> {
    doc.get("secret")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|a| a.iter())
}

fn entries(doc: &DocumentMut, kind: Kind) -> impl Iterator<Item = &Table> {
    doc.get(kind.key())
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|a| a.iter())
}

fn name_of(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

/// What tells the entry apart from the others of its `kind`.
fn identity_of(kind: Kind, table: &Table) -> Option<&str> {
    table.get(kind.identity().key()).and_then(Item::as_str)
}

fn read_text(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn read(path: &Path) -> Result<DocumentMut> {
    read_text(path)?
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))
}

/// Checks the edited text as a config, then replaces the file in one step. A file
/// that is invalid already may be mended a step at a time: an edit that leaves it
/// with fewer problems than it had is let through, though it is not yet valid.
fn write(path: &Path, doc: &DocumentMut) -> Result<()> {
    let text = doc.to_string();
    if let Err(refused) = Config::parse(&text, path)
        && !mends(path, &text)
    {
        return Err(refused);
    }
    store(path, &text)
}

/// Whether `text` has fewer problems than the file at `path` has now.
fn mends(path: &Path, text: &str) -> bool {
    let Ok(now) = read_text(path) else {
        return false;
    };
    let before = Config::problem_count(&now, path);
    before > 0 && Config::problem_count(text, path) < before
}

fn store(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut staged = path.as_os_str().to_owned();
    staged.push(".tmp");
    let staged = std::path::PathBuf::from(staged);
    std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn file(text: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new("config-edit").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn a_bundle_is_listed_once_and_what_else_is_written_stays() {
        let (_dir, path) = file("# mine\ntarget = \"docker\"\n\n[session]\nshell = \"/bin/zsh\"\n");
        assert!(add_bundle(&path, "/a.tar").unwrap());
        assert!(add_bundle(&path, "oci://r/x:1").unwrap());
        assert!(!add_bundle(&path, "/a.tar").unwrap(), "listed already");
        assert_eq!(bundles(&path).unwrap(), ["/a.tar", "oci://r/x:1"]);
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now.starts_with("# mine\ntarget"), "{now}");
        assert!(now.contains("[session]\nshell = \"/bin/zsh\""), "{now}");
        // Still a config lodge loads, with the list before the first table.
        let config = Config::load(&path).unwrap();
        assert_eq!(config.bundles, ["/a.tar", "oci://r/x:1"]);
        assert!(
            now.find("bundles").unwrap() < now.find("[session]").unwrap(),
            "{now}"
        );
    }

    #[test]
    fn taking_the_last_bundle_off_the_list_takes_the_key_out() {
        let (_dir, path) = file("bundles = [\"/a.tar\", \"/b.tar\"]\n");
        assert!(remove_bundle(&path, "/a.tar").unwrap());
        assert!(
            !remove_bundle(&path, "/a.tar").unwrap(),
            "not there any more"
        );
        assert_eq!(bundles(&path).unwrap(), ["/b.tar"]);
        assert!(remove_bundle(&path, "/b.tar").unwrap());
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(!now.contains("bundles"), "{now}");
    }

    #[test]
    fn listing_a_bundle_in_a_file_that_is_not_there_makes_it() {
        let dir = TempDir::new("config-edit-new").unwrap();
        let path = dir.path().join("lodge").join("config.toml");
        assert!(bundles(&path).unwrap().is_empty());
        add_bundle(&path, "/a.tar").unwrap();
        assert_eq!(bundles(&path).unwrap(), ["/a.tar"]);
    }

    #[test]
    fn an_entry_is_added_and_everything_else_stays_as_written() {
        let text = "# mine\ntarget = \"docker\"\n\n# the editor\n[[binary]]\nname = \"nvim\"\npath = \"/usr/bin/nvim\"\n\n[session]\nshell = \"/bin/zsh\"\n";
        let (_dir, path) = file(text);
        add(&path, &NewEntry::binary("rg", "/usr/bin/rg".into())).unwrap();
        add(
            &path,
            &NewEntry::data("~/.config/nvim".into(), None, Some("nvim".into())),
        )
        .unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now.starts_with("# mine\ntarget"), "{now}");
        assert!(now.contains("# the editor\n[[binary]]"), "{now}");
        assert!(
            now.contains("name = \"rg\"\npath = \"/usr/bin/rg\""),
            "{now}"
        );
        assert!(
            now.contains("[[data]]\npath = \"~/.config/nvim\"\nfor = \"nvim\""),
            "{now}"
        );
        let config = Config::load(&path).unwrap();
        assert_eq!((config.binaries.len(), config.data.len()), (2, 1));
        assert_eq!(config.session.shell, "/bin/zsh");
    }

    #[test]
    fn a_missing_file_is_created_and_a_comments_only_file_keeps_its_comments_on_top() {
        let dir = TempDir::new("config-edit-new").unwrap();
        let path = dir.path().join("lodge/config.toml");
        let e = NewEntry::Binary(NewBinary {
            name: "rg".into(),
            path: "/usr/bin/rg".into(),
            target: None,
            copy_deps: false,
        });
        add(&path, &e).unwrap();
        assert!(!Config::load(&path).unwrap().binaries[0].copy_deps);
        let (_dir, path) = file("# a starting point\n# [[binary]]\n");
        add(&path, &NewEntry::binary("rg", "/usr/bin/rg".into())).unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(
            now.starts_with("# a starting point\n# [[binary]]\n"),
            "{now}"
        );
    }

    #[test]
    fn an_entry_is_refused_when_its_name_or_its_path_is_taken() {
        let (_dir, path) = file("[[binary]]\nname = \"nvim\"\npath = \"/usr/bin/nvim\"\n");
        add(&path, &NewEntry::data("/etc/nvim".into(), None, None)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let err = add(&path, &NewEntry::binary("nvim", "/x".into())).unwrap_err();
        assert!(err.to_string().contains("already in"), "{err:#}");
        let err = add(&path, &NewEntry::data("/etc/nvim".into(), None, None)).unwrap_err();
        assert!(err.to_string().contains("[[data]] `/etc/nvim`"), "{err:#}");
        // Two paths to one place, a target the container user can't use, and a
        // binary that is not there: refused, and nothing is written.
        for bad in [
            NewEntry::data("/other".into(), Some("/etc/nvim".into()), None),
            NewEntry::data("/x".into(), Some("../out".into()), None),
            NewEntry::data("/y".into(), None, Some("emacs".into())),
        ] {
            assert!(add(&path, &bad).is_err());
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn an_entry_is_removed_by_its_name_or_path_with_its_neighbours_left_alone() {
        let text = "[[binary]]\nname = \"a\"\npath = \"/a\"\n\n[[data]]\npath = \"/da\"\n\n[[binary]]\nname = \"b\"\npath = \"/b\"\n\n[session]\nshell = \"/bin/sh\"\n";
        let (_dir, path) = file(text);
        remove(&path, Kind::Binary, "a").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.binaries.len(), 1);
        assert_eq!(config.data.len(), 1, "the data stays");
        remove(&path, Kind::Binary, "b").unwrap();
        remove(&path, Kind::Data, "/da").unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(!now.contains("[[") && now.contains("[session]"), "{now}");
        let err = remove(&path, Kind::Binary, "b").unwrap_err();
        assert!(err.to_string().contains("no [[binary]] `b`"), "{err:#}");
    }

    #[test]
    fn the_data_of_a_binary_follows_its_rename_and_outlives_its_removal() {
        let text = "[[binary]]\nname = \"nvim\"\npath = \"/usr/bin/nvim\"\n\n\
                    [[data]]\npath = \"/usr/share/nvim\"\nfor = \"nvim\"\n\n\
                    [[data]]\npath = \"/etc/x\"\n";
        let (_dir, path) = file(text);
        set_field(&path, Kind::Binary, "nvim", Field::Name, Some("vi")).unwrap();
        let data = list(&path, Kind::Data).unwrap();
        assert_eq!(data[0].binary.as_deref(), Some("vi"));
        assert_eq!(data[1].binary, None);
        remove(&path, Kind::Binary, "vi").unwrap();
        let data = list(&path, Kind::Data).unwrap();
        assert_eq!(data.len(), 2, "no data is lost with the binary");
        assert!(data.iter().all(|d| d.binary.is_none()));
        // `for` can only name a binary that is there.
        let err = set_field(&path, Kind::Data, "/etc/x", Field::For, Some("vi")).unwrap_err();
        assert!(format!("{err:#}").contains("not a [[binary]]"), "{err:#}");
    }

    #[test]
    fn entries_are_read_as_written_and_a_field_is_set_cleared_or_refused() {
        let text = "# keep\n[[binary]]\nname = \"a\"\npath = \"~/bin/a\"\ncopy_deps = false\n\n[[data]]\npath = \"/etc/b\"\n";
        let (_dir, path) = file(text);
        let binaries = list(&path, Kind::Binary).unwrap();
        assert_eq!(binaries[0].path.as_deref(), Some("~/bin/a"));
        assert_eq!(binaries[0].copy_deps, Some(false));
        assert_eq!(binaries[0].key(), "a");
        let data = list(&path, Kind::Data).unwrap();
        assert_eq!((data[0].copy_deps, data[0].key()), (None, "/etc/b"));
        set_field(&path, Kind::Data, "/etc/b", Field::Target, Some("~/.b")).unwrap();
        set_copy_deps(&path, "a", true).unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now.starts_with("# keep"), "{now}");
        assert!(!now.contains("copy_deps"), "{now}");
        assert!(now.contains("target = \"~/.b\""), "{now}");
        set_field(&path, Kind::Data, "/etc/b", Field::Target, None).unwrap();
        // The path or name taken out, a name in use, a target that is not usable, a
        // missing entry, a field the kind does not have, and text for copy_deps:
        // refused, the file left as it was.
        add(&path, &NewEntry::binary("c", "/c".into())).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        for result in [
            set_field(&path, Kind::Binary, "a", Field::Path, None),
            set_field(&path, Kind::Binary, "a", Field::Name, None),
            set_field(&path, Kind::Binary, "a", Field::Name, Some("c")),
            set_field(&path, Kind::Binary, "a", Field::Target, Some("../x")),
            set_field(&path, Kind::Binary, "zz", Field::Path, Some("/x")),
            set_field(&path, Kind::Binary, "a", Field::CopyDeps, Some("no")),
            set_field(&path, Kind::Binary, "a", Field::For, Some("c")),
            set_field(&path, Kind::Data, "/etc/b", Field::Name, Some("b")),
            set_field(&path, Kind::Data, "/etc/b", Field::Path, None),
        ] {
            assert!(result.is_err());
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        set_field(&path, Kind::Binary, "a", Field::Name, Some("d")).unwrap();
        assert_eq!(list(&path, Kind::Binary).unwrap()[0].key(), "d");
    }

    #[test]
    fn a_config_that_does_not_load_is_mended_by_taking_out_what_it_does_not_know() {
        let text = "# mine\ntarget = \"docker\"\n\n[[config]]\nname = \"nvim\"\n\n[[binary]]\nname = \"a\"\npath = \"/a\"\ncopy_dep = true\n";
        let (_dir, path) = file(text);
        // The edits a valid config allows are refused for one that is not.
        assert!(add(&path, &NewEntry::binary("b", "/b".into())).is_err());
        let all = unknown(&path).unwrap();
        assert_eq!(all.len(), 2);
        // Taking one out is allowed though the file is not yet valid, and the rest of
        // the text is as it was.
        remove_unknown(&path, &all[0]).unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(
            now.starts_with("# mine\ntarget") && now.contains("copy_dep"),
            "{now}"
        );
        assert!(Config::load(&path).is_err());
        // What is no longer there cannot be taken out again.
        assert!(remove_unknown(&path, &all[0]).is_err());
        remove_unknown(&path, &unknown(&path).unwrap()[0]).unwrap();
        Config::load(&path).unwrap();
        add(&path, &NewEntry::binary("b", "/b".into())).unwrap();
    }

    #[test]
    fn a_file_with_two_problems_is_mended_one_edit_at_a_time_and_never_made_worse() {
        let text = "[[binary]]\nname = \"a\"\npath = \"/a\"\ntarget = \"rel/a\"\n\n\
                    [[data]]\npath = \"/d\"\nfor = \"nope\"\n";
        let (_dir, path) = file(text);
        assert_eq!(Config::problem_count(text, &path), 2);
        // Not an edit that leaves it as bad, nor one that adds a problem...
        assert!(set_field(&path, Kind::Binary, "a", Field::Path, Some("/b")).is_err());
        assert!(add(&path, &NewEntry::binary("c", "/c".into())).is_err());
        // ...but each that takes one away.
        set_field(&path, Kind::Binary, "a", Field::Target, Some("~/bin/a")).unwrap();
        assert_eq!(
            Config::problem_count(&std::fs::read_to_string(&path).unwrap(), &path),
            1
        );
        set_field(&path, Kind::Data, "/d", Field::For, None).unwrap();
        Config::load(&path).unwrap();
    }

    #[test]
    fn settings_are_read_set_and_put_back_to_their_default() {
        let (_dir, path) = file("# mine\n[[binary]]\nname = \"a\"\npath = \"/a\"\n");
        assert_eq!(setting(&path, Setting::Shell).unwrap(), None);
        set_setting(&path, Setting::Target, Some("host")).unwrap();
        set_setting(&path, Setting::Shell, Some("/bin/zsh")).unwrap();
        set_setting(&path, Setting::SafeDirs, Some("/work, /srv")).unwrap();
        assert_eq!(
            setting(&path, Setting::Target).unwrap().as_deref(),
            Some("\"host\"")
        );
        assert_eq!(
            setting(&path, Setting::Shell).unwrap().as_deref(),
            Some("\"/bin/zsh\"")
        );
        assert_eq!(
            setting(&path, Setting::SafeDirs).unwrap().as_deref(),
            Some("[\"/work\", \"/srv\"]")
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(config.session.shell, "/bin/zsh");
        assert_eq!(config.git.safe_directories, vec!["/work", "/srv"]);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# mine\n[[binary]]")
        );
        // `none` switches the setting off; taking a setting out puts back the
        // default, and a table left empty goes with it.
        set_setting(&path, Setting::SafeDirs, Some("none")).unwrap();
        assert!(Config::load(&path).unwrap().git.safe_directories.is_empty());
        set_setting(&path, Setting::SafeDirs, None).unwrap();
        set_setting(&path, Setting::Shell, None).unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(
            !now.contains("[session]") && !now.contains("[git]"),
            "{now}"
        );
        set_setting(&path, Setting::Target, None).unwrap();
        assert_eq!(setting(&path, Setting::Target).unwrap(), None);
    }

    #[test]
    fn secrets_are_added_changed_and_removed_as_inline_tables() {
        let (_dir, path) = file("");
        let new = NewSecret {
            name: "GH_TOKEN".into(),
            from: SecretFrom::Command("gh auth token".into()),
            as_: SecretAs::Env,
        };
        add_secret(&path, &new).unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(
            now.contains("from = { command = \"gh auth token\" }"),
            "{now}"
        );
        assert!(now.contains("as = \"env\""), "{now}");
        assert!(add_secret(&path, &new).is_err(), "a name in use");
        set_secret_from(&path, "GH_TOKEN", &SecretFrom::Env("GITHUB_TOKEN".into())).unwrap();
        set_secret_as(
            &path,
            "GH_TOKEN",
            &SecretAs::File {
                path: "~/.gh".into(),
                mode: Some("0600".into()),
            },
        )
        .unwrap();
        set_secret_targets(&path, "GH_TOKEN", &["me@box".into()]).unwrap();
        let listed = list_secrets(&path).unwrap();
        assert_eq!(
            listed[0].from.as_deref(),
            Some("{ env = \"GITHUB_TOKEN\" }")
        );
        assert_eq!(
            listed[0].as_.as_deref(),
            Some("{ file = \"~/.gh\", mode = \"0600\" }")
        );
        assert_eq!(listed[0].targets.as_deref(), Some("[\"me@box\"]"));
        // What the secret checks refuse is refused here: nothing written.
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(rename_secret(&path, "GH_TOKEN", "not a name").is_err());
        assert!(set_secret_from(&path, "GH_TOKEN", &SecretFrom::Command(" ".into())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        set_secret_targets(&path, "GH_TOKEN", &[]).unwrap();
        rename_secret(&path, "GH_TOKEN", "TOKEN").unwrap();
        remove_secret(&path, "TOKEN").unwrap();
        assert!(list_secrets(&path).unwrap().is_empty());
        assert!(remove_secret(&path, "TOKEN").is_err());
    }
}
