//! Adding a program together with what sits beside it, for the two places that add
//! programs: `ldg inject add binary` and the config screen.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::candidates;
use crate::config::edit::{self, Kind, NewEntry};
use crate::config::{ConfigFile, expand_tilde};
use crate::layout::tilde;

/// Adds a program and, when `with_data`, what it keeps beside it (see
/// [`candidates::companions`]) as data for it, so it runs in the container as it
/// does here. Returns the paths of the data added, as written in the config; what
/// the config has already is not added again.
pub(super) fn add_binary(
    path: &ConfigFile,
    entry: NewEntry,
    home: &Path,
    with_data: bool,
) -> Result<Vec<String>> {
    let name = entry.key().to_string();
    let host = std::path::absolute(expand_tilde(PathBuf::from(entry.path())))?;
    edit::add(path, &entry)?;
    if !with_data {
        return Ok(Vec::new());
    }
    let have: Vec<String> = edit::list(path, Kind::Data)?
        .into_iter()
        .filter_map(|d| d.path)
        .collect();
    let mut added = Vec::new();
    for place in candidates::companions(&host) {
        let written = tilde(&place, home);
        if !have.contains(&written) {
            edit::add(
                path,
                &NewEntry::data(written.clone(), None, Some(name.clone())),
            )?;
            added.push(written);
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn prefix_with_share(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir.join("opt/bin")).unwrap();
        std::fs::create_dir_all(dir.join("opt/share/tool")).unwrap();
        std::fs::write(dir.join("opt/bin/tool"), "#!/bin/sh\n").unwrap();
        dir.join("opt/bin/tool")
    }

    #[test]
    fn a_program_is_added_with_what_sits_beside_it_and_nothing_twice() {
        let dir = TempDir::new("add-binary").unwrap();
        let tool = prefix_with_share(dir.path());
        let config = ConfigFile::new(dir.path().join("config.toml"), crate::files::disk());
        let entry = || NewEntry::binary("tool", tool.display().to_string());

        let added = add_binary(&config, entry(), dir.path(), true).unwrap();
        assert_eq!(added, ["~/opt/share/tool"]);
        let data = edit::list(&config, Kind::Data).unwrap();
        assert_eq!(data[0].binary.as_deref(), Some("tool"));

        // The same data is not added again for a program of another name.
        let other = NewEntry::binary("tool2", tool.display().to_string());
        assert!(
            add_binary(&config, other, dir.path(), true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(edit::list(&config, Kind::Data).unwrap().len(), 1);
    }

    #[test]
    fn a_program_can_be_added_alone() {
        let dir = TempDir::new("add-binary-alone").unwrap();
        let tool = prefix_with_share(dir.path());
        let config = ConfigFile::new(dir.path().join("config.toml"), crate::files::disk());
        let entry = NewEntry::binary("tool", tool.display().to_string());
        assert!(
            add_binary(&config, entry, dir.path(), false)
                .unwrap()
                .is_empty()
        );
        assert!(edit::list(&config, Kind::Data).unwrap().is_empty());
    }
}
