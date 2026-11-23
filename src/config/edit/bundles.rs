//! The list of other bundles on offer, `bundles` in the client file.

use anyhow::Result;
use toml_edit::{Array, Item, value};

use super::{read, write};
use crate::config::ClientFile;

/// The bundles listed under `bundles`, as written.
pub fn bundles(path: &ClientFile) -> Result<Vec<String>> {
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
pub fn add_bundle(path: &ClientFile, reference: &str) -> Result<bool> {
    let mut list = bundles(path)?;
    if list.iter().any(|b| b == reference) {
        return Ok(false);
    }
    list.push(reference.to_string());
    set_bundles(path, &list)?;
    Ok(true)
}

/// Takes `reference` out of `bundles`. `false` when it was not there.
pub fn remove_bundle(path: &ClientFile, reference: &str) -> Result<bool> {
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
pub fn set_bundles(path: &ClientFile, list: &[String]) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::edit::testing::{client_file, read_back};
    use crate::files::MemoryFiles;

    // TODO(windows): `/a.tar` is read back as `C:/a.tar` there; assert on a path
    // the platform calls absolute.
    #[cfg(unix)]
    #[test]
    fn a_bundle_is_listed_once_and_what_else_is_written_stays() {
        let (_dir, path) = client_file(
            "# mine\ndefault_target = \"docker\"\n\n[targets.docker]\ncontext = \"x\"\n",
        );
        assert!(add_bundle(&path, "/a.tar").unwrap());
        assert!(add_bundle(&path, "oci://r/x:1").unwrap());
        assert!(!add_bundle(&path, "/a.tar").unwrap(), "listed already");
        assert_eq!(bundles(&path).unwrap(), ["/a.tar", "oci://r/x:1"]);
        let now = read_back(&path);
        assert!(now.starts_with("# mine\ndefault_target"), "{now}");
        assert!(now.contains("[targets.docker]\ncontext = \"x\""), "{now}");
        // Still a client file lodge loads, with the list before the first table.
        let settings = path.load().unwrap();
        assert_eq!(settings.bundles, ["/a.tar", "oci://r/x:1"]);
        assert!(
            now.find("bundles").unwrap() < now.find("[targets.docker]").unwrap(),
            "{now}"
        );
    }

    #[test]
    fn taking_the_last_bundle_off_the_list_takes_the_key_out() {
        let (_dir, path) = client_file("bundles = [\"/a.tar\", \"/b.tar\"]\n");
        assert!(remove_bundle(&path, "/a.tar").unwrap());
        assert!(
            !remove_bundle(&path, "/a.tar").unwrap(),
            "not there any more"
        );
        assert_eq!(bundles(&path).unwrap(), ["/b.tar"]);
        assert!(remove_bundle(&path, "/b.tar").unwrap());
        let now = read_back(&path);
        assert!(!now.contains("bundles"), "{now}");
    }

    #[test]
    fn listing_a_bundle_in_a_file_that_is_not_there_makes_it() {
        let path = ClientFile::new("/c/lodge/client.toml", MemoryFiles::new());
        assert!(bundles(&path).unwrap().is_empty());
        add_bundle(&path, "/a.tar").unwrap();
        assert_eq!(bundles(&path).unwrap(), ["/a.tar"]);
    }
}
