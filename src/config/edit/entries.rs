//! The `[[binary]]` and `[[data]]` entries of the config file.

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

use super::{read, write};
use crate::config::ConfigFile;

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
pub fn add(path: &ConfigFile, entry: &NewEntry) -> Result<()> {
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
pub fn remove(path: &ConfigFile, kind: Kind, key: &str) -> Result<()> {
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
pub fn list(path: &ConfigFile, kind: Kind) -> Result<Vec<RawEntry>> {
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

/// Sets a text field of the entry of `kind` known as `key`, or takes it out with
/// `None` (`copy_deps` is not text: see [`set_copy_deps`]). The result is checked as
/// a whole, so a name in use or a target the container can't use is refused. The
/// data that is for a binary follows it when the binary is renamed.
pub fn set_field(
    path: &ConfigFile,
    kind: Kind,
    key: &str,
    field: Field,
    to: Option<&str>,
) -> Result<()> {
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
pub fn set_copy_deps(path: &ConfigFile, name: &str, on: bool) -> Result<()> {
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
    path: &ConfigFile,
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

fn entries(doc: &DocumentMut, kind: Kind) -> impl Iterator<Item = &Table> {
    doc.get(kind.key())
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|a| a.iter())
}

/// What tells the entry apart from the others of its `kind`.
fn identity_of(kind: Kind, table: &Table) -> Option<&str> {
    table.get(kind.identity().key()).and_then(Item::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::edit::testing::{file, read_back};
    use crate::files::MemoryFiles;

    #[test]
    fn an_entry_is_added_and_everything_else_stays_as_written() {
        let text = "# mine\n[git]\nsafe_directories = [\"/w\"]\n\n# the editor\n[[binary]]\nname = \"nvim\"\npath = \"/usr/bin/nvim\"\n\n[session]\nshell = \"/bin/zsh\"\n";
        let (_dir, path) = file(text);
        add(&path, &NewEntry::binary("rg", "/usr/bin/rg".into())).unwrap();
        add(
            &path,
            &NewEntry::data("~/.config/nvim".into(), None, Some("nvim".into())),
        )
        .unwrap();
        let now = read_back(&path);
        assert!(now.starts_with("# mine\n[git]"), "{now}");
        assert!(now.contains("# the editor\n[[binary]]"), "{now}");
        assert!(
            now.contains("name = \"rg\"\npath = \"/usr/bin/rg\""),
            "{now}"
        );
        assert!(
            now.contains("[[data]]\npath = \"~/.config/nvim\"\nfor = \"nvim\""),
            "{now}"
        );
        let config = path.load().unwrap();
        assert_eq!((config.binaries.len(), config.data.len()), (2, 1));
        assert_eq!(config.session.shell, "/bin/zsh");
    }

    #[test]
    fn a_missing_file_is_created_and_a_comments_only_file_keeps_its_comments_on_top() {
        let path = ConfigFile::new("/c/lodge/config.toml", MemoryFiles::new());
        let e = NewEntry::Binary(NewBinary {
            name: "rg".into(),
            path: "/usr/bin/rg".into(),
            target: None,
            copy_deps: false,
        });
        add(&path, &e).unwrap();
        assert!(!path.load().unwrap().binaries[0].copy_deps);
        let (_dir, path) = file("# a starting point\n# [[binary]]\n");
        add(&path, &NewEntry::binary("rg", "/usr/bin/rg".into())).unwrap();
        let now = read_back(&path);
        assert!(
            now.starts_with("# a starting point\n# [[binary]]\n"),
            "{now}"
        );
    }

    #[test]
    fn an_entry_is_refused_when_its_name_or_its_path_is_taken() {
        let (_dir, path) = file("[[binary]]\nname = \"nvim\"\npath = \"/usr/bin/nvim\"\n");
        add(&path, &NewEntry::data("/etc/nvim".into(), None, None)).unwrap();
        let text = read_back(&path);
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
        assert_eq!(read_back(&path), text);
    }

    #[test]
    fn an_entry_is_removed_by_its_name_or_path_with_its_neighbours_left_alone() {
        let text = "[[binary]]\nname = \"a\"\npath = \"/a\"\n\n[[data]]\npath = \"/da\"\n\n[[binary]]\nname = \"b\"\npath = \"/b\"\n\n[session]\nshell = \"/bin/sh\"\n";
        let (_dir, path) = file(text);
        remove(&path, Kind::Binary, "a").unwrap();
        let config = path.load().unwrap();
        assert_eq!(config.binaries.len(), 1);
        assert_eq!(config.data.len(), 1, "the data stays");
        remove(&path, Kind::Binary, "b").unwrap();
        remove(&path, Kind::Data, "/da").unwrap();
        let now = read_back(&path);
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
        let now = read_back(&path);
        assert!(now.starts_with("# keep"), "{now}");
        assert!(!now.contains("copy_deps"), "{now}");
        assert!(now.contains("target = \"~/.b\""), "{now}");
        set_field(&path, Kind::Data, "/etc/b", Field::Target, None).unwrap();
        // The path or name taken out, a name in use, a target that is not usable, a
        // missing entry, a field the kind does not have, and text for copy_deps:
        // refused, the file left as it was.
        add(&path, &NewEntry::binary("c", "/c".into())).unwrap();
        let before = read_back(&path);
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
        assert_eq!(read_back(&path), before);
        set_field(&path, Kind::Binary, "a", Field::Name, Some("d")).unwrap();
        assert_eq!(list(&path, Kind::Binary).unwrap()[0].key(), "d");
    }
}
