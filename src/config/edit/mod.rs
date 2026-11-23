//! Changing the `[[binary]]` and `[[data]]` entries of the config file
//! ([`mod@entries`]), the settings of either file ([`mod@settings`]), the list of bundles
//! ([`mod@bundles`]) and the `[[secret]]` entries of the client file ([`mod@secrets`]), from
//! the command line and the config screen. A function works on the file it is given
//! and checks it as the kind of file it is: the functions for entries take a
//! [`crate::config::ConfigFile`], those for secrets and bundles a [`crate::config::ClientFile`], and one for a
//! setting the [`crate::config::Paths`], which finds the file the setting belongs in.
//!
//! The file is edited as text (comments, order and layout stay as the person
//! wrote them), and what would be written is parsed and checked as a whole
//! before the file is touched, so an edit can never leave a config lodge
//! refuses to load. What every kind of edit shares is here: reading a file as an
//! editable document, writing it back checked, and taking out what lodge does not
//! know.

mod bundles;
mod entries;
mod secrets;
mod settings;

pub use bundles::*;
pub use entries::*;
pub use secrets::*;
pub use settings::*;

use anyhow::{Context, Result};
use toml_edit::DocumentMut;

use super::Document;
use super::unknown::{self, Unknown};

/// What the config at `path` holds that this version of lodge does not know (none
/// when it is missing).
pub fn unknown(file: &dyn Document) -> Result<Vec<Unknown>> {
    Ok(unknown::find(&text(file)?, file))
}

/// Takes `which` out of the config at `path`. It is the one edit that needs no
/// valid config as its result: what it takes out is what keeps one from loading.
pub fn remove_unknown(file: &dyn Document, which: &Unknown) -> Result<()> {
    let changed = unknown::remove(&text(file)?, which, file).with_context(|| {
        format!(
            "`{}` is no longer in {}",
            which.label,
            file.path().display()
        )
    })?;
    file.write_text(&changed)
}

/// What the file holds; nothing, when it is not there.
fn text(file: &dyn Document) -> Result<String> {
    Ok(file.read_text()?.unwrap_or_default())
}

fn read(file: &dyn Document) -> Result<DocumentMut> {
    text(file)?
        .parse()
        .with_context(|| format!("{} is not valid TOML", file.path().display()))
}

/// Checks the edited text as a config, then replaces the file in one step. A file
/// that is invalid already may be mended a step at a time: an edit that leaves it
/// with fewer problems than it had is let through, though it is not yet valid.
fn write(file: &dyn Document, doc: &DocumentMut) -> Result<()> {
    let now = doc.to_string();
    if let Err(refused) = file.check(&now)
        && !mends(file, &now)
    {
        return Err(refused);
    }
    file.write_text(&now)
}

/// Whether `edited` has fewer problems than the file has now.
fn mends(file: &dyn Document, edited: &str) -> bool {
    let Ok(now) = text(file) else {
        return false;
    };
    let before = file.problem_count(&now);
    before > 0 && file.problem_count(edited) < before
}

#[cfg(test)]
mod testing {
    use super::*;
    use crate::config::{ClientFile, ConfigFile};
    use crate::files::MemoryFiles;
    use std::sync::Arc;

    /// A config holding `text`, kept in memory.
    pub fn file(text: &str) -> (Arc<MemoryFiles>, ConfigFile) {
        let files = MemoryFiles::new().with("/c/config.toml", text);
        (
            files.clone(),
            ConfigFile::new("/c/config.toml", files.clone()),
        )
    }

    /// A client file holding `text`, kept in memory: checked as one because it is one.
    pub fn client_file(text: &str) -> (Arc<MemoryFiles>, ClientFile) {
        let files = MemoryFiles::new().with("/c/client.toml", text);
        (
            files.clone(),
            ClientFile::new("/c/client.toml", files.clone()),
        )
    }

    /// What the file holds now.
    pub fn read_back(file: &dyn Document) -> String {
        file.read_text().unwrap().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{file, read_back};
    use super::*;
    use crate::config::edit::NewEntry;
    use crate::config::{Config, Paths};
    use crate::files::MemoryFiles;

    #[test]
    fn a_config_that_does_not_load_is_mended_by_taking_out_what_it_does_not_know() {
        let text = "# mine\n[git]\nsafe_directories = [\"/w\"]\nbogus = 1\n\n[[config]]\nname = \"nvim\"\n\n[[binary]]\nname = \"a\"\npath = \"/a\"\ncopy_dep = true\n";
        let (_dir, path) = file(text);
        // The edits a valid config allows are refused for one that is not.
        assert!(add(&path, &NewEntry::binary("b", "/b".into())).is_err());
        let all = unknown(&path).unwrap();
        assert_eq!(all.len(), 3);
        // Taking one out is allowed though the file is not yet valid, and the rest of
        // the text is as it was.
        remove_unknown(&path, &all[0]).unwrap();
        let now = read_back(&path);
        assert!(
            now.starts_with("# mine\n[git]") && now.contains("copy_dep"),
            "{now}"
        );
        assert!(path.load().is_err());
        // What is no longer there cannot be taken out again.
        assert!(remove_unknown(&path, &all[0]).is_err());
        remove_unknown(&path, &unknown(&path).unwrap()[0]).unwrap();
        assert!(path.load().is_err());
        remove_unknown(&path, &unknown(&path).unwrap()[0]).unwrap();
        path.load().unwrap();
        add(&path, &NewEntry::binary("b", "/b".into())).unwrap();
    }

    #[test]
    fn a_file_with_two_problems_is_mended_one_edit_at_a_time_and_never_made_worse() {
        let text = "[[binary]]\nname = \"a\"\npath = \"/a\"\ntarget = \"rel/a\"\n\n\
                    [[data]]\npath = \"/d\"\nfor = \"nope\"\n";
        let (_dir, path) = file(text);
        assert_eq!(Config::problem_count(text), 2);
        // Not an edit that leaves it as bad, nor one that adds a problem...
        assert!(set_field(&path, Kind::Binary, "a", Field::Path, Some("/b")).is_err());
        assert!(add(&path, &NewEntry::binary("c", "/c".into())).is_err());
        // ...but each that takes one away.
        set_field(&path, Kind::Binary, "a", Field::Target, Some("~/bin/a")).unwrap();
        assert_eq!(Config::problem_count(&read_back(&path)), 1);
        set_field(&path, Kind::Data, "/d", Field::For, None).unwrap();
        path.load().unwrap();
    }

    #[test]
    fn a_disk_that_refuses_is_an_error_that_names_the_file_and_changes_nothing() {
        let (files, path) = file("[[binary]]\nname = \"a\"\npath = \"/a\"\n");
        let files = files.refusing("disk full");
        let before = read_back(&path);
        let e = add(&path, &NewEntry::binary("b", "/b".into())).unwrap_err();
        let said = format!("{e:#}");
        assert!(
            said.contains("writing /c/config.toml") && said.contains("disk full"),
            "{said}"
        );
        assert_eq!(read_back(&path), before);
        assert_eq!(files.writes(), 0);
    }

    #[test]
    fn nothing_is_written_when_the_result_would_not_load() {
        let (files, path) = file("[[binary]]\nname = \"a\"\npath = \"/a\"\n");
        assert!(add(&path, &NewEntry::binary("a", "/other".into())).is_err());
        assert_eq!(files.writes(), 0, "a refused edit never reaches the disk");
        add(&path, &NewEntry::binary("b", "/b".into())).unwrap();
        assert_eq!(files.writes(), 1);
    }

    #[test]
    fn a_config_and_a_client_file_kept_in_one_place_stay_each_in_its_own_file() {
        let files = MemoryFiles::new();
        let paths = Paths::beside("/c/config.toml".into(), files.clone());
        add(&paths.config, &NewEntry::binary("a", "/a".into())).unwrap();
        add_bundle(&paths.client, "/b.tar").unwrap();
        assert!(files.text("/c/config.toml").unwrap().contains("[[binary]]"));
        assert!(!files.text("/c/config.toml").unwrap().contains("bundles"));
        assert!(files.text("/c/client.toml").unwrap().contains("bundles"));
        assert_eq!(paths.config.load().unwrap().binaries.len(), 1);
        assert_eq!(paths.client.load().unwrap().bundles, ["/b.tar"]);
    }
}
