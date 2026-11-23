//! The single values of the two files: `target` and `bundle` of the client's, `shell`
//! and `safe_directories` of the config's.

use anyhow::Result;
use toml_edit::{Array, Item, Table, value};

use super::{read, write};
use crate::config::{Document, Paths};

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

/// The file a setting belongs in: the client's own for `target` and `bundle`, the
/// config's for the rest. The one place that knows.
fn home(paths: &Paths, which: Setting) -> &dyn Document {
    match which {
        Setting::Target | Setting::Bundle => &paths.client,
        Setting::Shell | Setting::SafeDirs => &paths.config,
    }
}

/// A setting as it is written in the file (a quoted string, or a list like `["*"]`), none when it is
/// not there.
pub fn setting(paths: &Paths, which: Setting) -> Result<Option<String>> {
    let doc = read(home(paths, which))?;
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
pub fn set_setting(paths: &Paths, which: Setting, to: Option<&str>) -> Result<()> {
    let file = home(paths, which);
    let mut doc = read(file)?;
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
    write(file, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::edit::testing::{file, read_back};

    #[test]
    fn settings_are_read_set_and_put_back_to_their_default() {
        let (files, path) = file("# mine\n[[binary]]\nname = \"a\"\npath = \"/a\"\n");
        let paths = Paths::beside("/c/config.toml".into(), files);
        assert_eq!(setting(&paths, Setting::Shell).unwrap(), None);
        set_setting(&paths, Setting::Shell, Some("/bin/zsh")).unwrap();
        set_setting(&paths, Setting::SafeDirs, Some("/work, /srv")).unwrap();
        assert_eq!(
            setting(&paths, Setting::Shell).unwrap().as_deref(),
            Some("\"/bin/zsh\"")
        );
        assert_eq!(
            setting(&paths, Setting::SafeDirs).unwrap().as_deref(),
            Some("[\"/work\", \"/srv\"]")
        );
        let config = path.load().unwrap();
        assert_eq!(config.session.shell, "/bin/zsh");
        assert_eq!(config.git.safe_directories, vec!["/work", "/srv"]);
        assert!(read_back(&path).contains("# mine\n[[binary]]"));
        // `none` switches the setting off; taking a setting out puts back the
        // default, and a table left empty goes with it.
        set_setting(&paths, Setting::SafeDirs, Some("none")).unwrap();
        assert!(path.load().unwrap().git.safe_directories.is_empty());
        set_setting(&paths, Setting::SafeDirs, None).unwrap();
        set_setting(&paths, Setting::Shell, None).unwrap();
        let now = read_back(&path);
        assert!(
            !now.contains("[session]") && !now.contains("[git]"),
            "{now}"
        );
    }

    #[test]
    fn a_setting_is_written_into_the_file_it_belongs_in() {
        let (files, _) = file("");
        let paths = Paths::beside("/c/config.toml".into(), files);
        set_setting(&paths, Setting::Target, Some("host")).unwrap();
        set_setting(&paths, Setting::Bundle, Some("/b.tar")).unwrap();
        set_setting(&paths, Setting::Shell, Some("/bin/zsh")).unwrap();
        assert_eq!(
            setting(&paths, Setting::Target).unwrap().as_deref(),
            Some("\"host\"")
        );
        // The client's own went to its file, the config's to the config: neither
        // file holds the other's.
        let client_text = read_back(&paths.client);
        let config_text = read_back(&paths.config);
        assert!(client_text.contains("target") && client_text.contains("bundle"));
        assert!(!client_text.contains("shell"), "{client_text}");
        assert!(config_text.contains("shell") && !config_text.contains("target"));
        assert_eq!(paths.client.load().unwrap().target.as_deref(), Some("host"));
        set_setting(&paths, Setting::Target, None).unwrap();
        assert_eq!(setting(&paths, Setting::Target).unwrap(), None);
    }
}
