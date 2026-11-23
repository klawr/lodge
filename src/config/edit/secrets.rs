//! The `[[secret]]` entries of the client file.

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use super::{read, write};
use crate::config::ClientFile;

/// Where a secret's value comes from, on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretFrom {
    Env(String),
    File(String),
    Command(String),
    /// An entry of this machine's credential store (`ldg secret set`).
    Store(String),
}

impl SecretFrom {
    pub fn kind(&self) -> &'static str {
        match self {
            SecretFrom::Env(_) => "env",
            SecretFrom::File(_) => "file",
            SecretFrom::Command(_) => "command",
            SecretFrom::Store(_) => "store",
        }
    }

    fn item(&self) -> Item {
        let (kind, text) = match self {
            SecretFrom::Env(v)
            | SecretFrom::File(v)
            | SecretFrom::Command(v)
            | SecretFrom::Store(v) => (self.kind(), v),
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

pub fn list_secrets(path: &ClientFile) -> Result<Vec<RawSecret>> {
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

pub fn add_secret(path: &ClientFile, secret: &NewSecret) -> Result<()> {
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

pub fn remove_secret(path: &ClientFile, name: &str) -> Result<()> {
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

pub fn rename_secret(path: &ClientFile, name: &str, to: &str) -> Result<()> {
    change_secret(path, name, |t| t["name"] = value(to))
}

pub fn set_secret_from(path: &ClientFile, name: &str, from: &SecretFrom) -> Result<()> {
    change_secret(path, name, |t| t["from"] = from.item())
}

pub fn set_secret_as(path: &ClientFile, name: &str, as_: &SecretAs) -> Result<()> {
    change_secret(path, name, |t| t["as"] = as_.item())
}

/// The hosts and container names a secret may go to; none means anywhere.
pub fn set_secret_targets(path: &ClientFile, name: &str, targets: &[String]) -> Result<()> {
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

fn change_secret(path: &ClientFile, name: &str, edit: impl FnOnce(&mut Table)) -> Result<()> {
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

fn name_of(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::edit::testing::{client_file, read_back};

    #[test]
    fn secrets_are_added_changed_and_removed_as_inline_tables() {
        let (_dir, path) = client_file("");
        let new = NewSecret {
            name: "GH_TOKEN".into(),
            from: SecretFrom::Command("gh auth token".into()),
            as_: SecretAs::Env,
        };
        add_secret(&path, &new).unwrap();
        let now = read_back(&path);
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
        let before = read_back(&path);
        assert!(rename_secret(&path, "GH_TOKEN", "not a name").is_err());
        assert!(set_secret_from(&path, "GH_TOKEN", &SecretFrom::Command(" ".into())).is_err());
        assert_eq!(read_back(&path), before);
        set_secret_targets(&path, "GH_TOKEN", &[]).unwrap();
        rename_secret(&path, "GH_TOKEN", "TOKEN").unwrap();
        remove_secret(&path, "TOKEN").unwrap();
        assert!(list_secrets(&path).unwrap().is_empty());
        assert!(remove_secret(&path, "TOKEN").is_err());
    }

    #[test]
    fn a_secret_can_read_from_the_credential_store() {
        let (_dir, path) = client_file("");
        let new = NewSecret {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            from: SecretFrom::Store("claude_token".into()),
            as_: SecretAs::Env,
        };
        add_secret(&path, &new).unwrap();
        let listed = list_secrets(&path).unwrap();
        assert_eq!(
            listed[0].from.as_deref(),
            Some("{ store = \"claude_token\" }")
        );
        // The checks of the config apply: an entry name a script cannot take is
        // refused, and nothing is written.
        let before = read_back(&path);
        let bad = SecretFrom::Store("not a name".into());
        assert!(set_secret_from(&path, "CLAUDE_CODE_OAUTH_TOKEN", &bad).is_err());
        assert_eq!(read_back(&path), before);
        // And it can be changed to another kind and back.
        set_secret_from(
            &path,
            "CLAUDE_CODE_OAUTH_TOKEN",
            &SecretFrom::Env("T".into()),
        )
        .unwrap();
        let back = SecretFrom::Store("other".into());
        set_secret_from(&path, "CLAUDE_CODE_OAUTH_TOKEN", &back).unwrap();
        assert_eq!(
            list_secrets(&path).unwrap()[0].from.as_deref(),
            Some("{ store = \"other\" }")
        );
    }
}
