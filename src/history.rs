//! The places `ldg` has opened a session in, most recent first, so a bare `ldg`
//! can offer them again.
//!
//! An [`Entry`] is *how a session was reached*: the [`Place`] (a project folder
//! or a container), the [`Route`] to it (the target and the SSH remote) and a name
//! to recognise it by. It holds no more than that, so resuming one is the same as
//! typing the command that made it, and nothing here knows what a command is.
//!
//! [`History`] is the seam: the command layer takes it, and a test hands in
//! a list in memory. [`FileHistory`] keeps the list in `~/.local/share/lodge/history.json`,
//! through [`Files`], so it is tested without a folder on disk.

use crate::files::Files;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How many places are kept; the least recent one makes room for a new one.
const MAX_ENTRIES: usize = 50;

/// How a place is reached: the target that was worked in, and the SSH remote the
/// engine was on (`None` for this machine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub target: String,
    pub remote: Option<String>,
}

/// Where the session was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Place {
    /// A project folder, on the route's host: `ldg up PATH`.
    Project { path: PathBuf },
    /// A container lodge did not make: `ldg attach NAME`.
    Container {
        name: String,
        /// The `--workdir` it was attached with, if one was given.
        workdir: Option<String>,
    },
}

impl Place {
    /// What to call it when it has no name of its own: the project's folder, or
    /// the container's name.
    pub fn fallback_name(&self) -> String {
        match self {
            Place::Project { path } => path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            ),
            Place::Container { name, .. } => name.clone(),
        }
    }

    /// Whether both are the same place; what a container was attached with is not
    /// part of which it is.
    fn is(&self, other: &Place) -> bool {
        match (self, other) {
            (Place::Project { path: a }, Place::Project { path: b }) => a == b,
            (Place::Container { name: a, .. }, Place::Container { name: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// One remembered session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub place: Place,
    pub route: Route,
    /// What the place calls itself (a devcontainer's `name`), else its folder or
    /// container name.
    pub name: String,
    /// When it was last opened, in seconds since the Unix epoch.
    pub last_used: u64,
    /// The bundle the session was injected from, as it was written: a reference,
    /// none for the config packed on the spot. Shown, not replayed: opening the
    /// place again uses the bundle the config names then.
    pub bundle: Option<String>,
}

impl Entry {
    pub fn is(&self, place: &Place, route: &Route) -> bool {
        self.place.is(place) && self.route == *route
    }
}

/// A list of the places sessions were opened in.
pub trait History {
    /// The remembered places, most recent first.
    fn entries(&self) -> Result<Vec<Entry>>;

    /// Notes that a session was opened in `place`: it becomes the most recent
    /// entry, replacing an earlier one for the same place and route.
    fn remember(
        &self,
        place: Place,
        route: Route,
        name: String,
        bundle: Option<String>,
    ) -> Result<()>;
}

/// Puts a session at the front of `entries`, in place of an earlier one for the
/// same place and route, and drops what no longer fits.
fn put_first(entries: &mut Vec<Entry>, entry: Entry) {
    entries.retain(|e| !e.is(&entry.place, &entry.route));
    entries.insert(0, entry);
    entries.truncate(MAX_ENTRIES);
}

/// The history kept in a JSON file.
///
/// Written whole through a temporary file and a rename, so a reader never sees half
/// a list. Two terminals recording at the same moment may lose one of the two
/// entries; nothing breaks, and the next session records it again.
pub struct FileHistory {
    /// `None` when this system has no data directory.
    path: Option<PathBuf>,
    files: Arc<dyn Files>,
}

impl FileHistory {
    /// `~/.local/share/lodge/history.json`, next to the port-forwarding state.
    pub fn in_data_dir(files: Arc<dyn Files>) -> Self {
        FileHistory {
            path: dirs::data_local_dir().map(|dir| dir.join("lodge").join("history.json")),
            files,
        }
    }

    #[cfg(test)]
    pub fn at(path: PathBuf, files: Arc<dyn Files>) -> Self {
        FileHistory {
            path: Some(path),
            files,
        }
    }

    fn file(&self) -> Result<&Path> {
        self.path
            .as_deref()
            .context("this system has no data directory to keep the history in")
    }
}

impl History for FileHistory {
    fn entries(&self) -> Result<Vec<Entry>> {
        let path = self.file()?;
        let Some(text) = self
            .files
            .read(path)
            .with_context(|| format!("reading {}", path.display()))?
        else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not a history lodge can read (remove it to start a new one)",
                path.display()
            )
        })
    }

    fn remember(
        &self,
        place: Place,
        route: Route,
        name: String,
        bundle: Option<String>,
    ) -> Result<()> {
        let path = self.file()?;
        // A history that can't be read is replaced rather than kept from being
        // written: it is only a convenience, and `entries` says what is wrong.
        let mut entries = self.entries().unwrap_or_default();
        let last_used = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        put_first(
            &mut entries,
            Entry {
                place,
                route,
                name,
                last_used,
                bundle,
            },
        );
        // Private to the user, like a shell history: it names project folders and hosts.
        self.files
            .write_private(path, &serde_json::to_string_pretty(&entries)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// A history that lives in memory, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Memory(std::cell::RefCell<Vec<Entry>>);

#[cfg(test)]
impl Memory {
    pub fn with(entries: Vec<Entry>) -> Self {
        Memory(std::cell::RefCell::new(entries))
    }
}

#[cfg(test)]
impl History for Memory {
    fn entries(&self) -> Result<Vec<Entry>> {
        Ok(self.0.borrow().clone())
    }

    fn remember(
        &self,
        place: Place,
        route: Route,
        name: String,
        bundle: Option<String>,
    ) -> Result<()> {
        put_first(
            &mut self.0.borrow_mut(),
            Entry {
                place,
                route,
                name,
                last_used: 0,
                bundle,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::MemoryFiles;

    const PATH: &str = "/data/lodge/history.json";

    fn kept_in(files: &Arc<MemoryFiles>) -> FileHistory {
        FileHistory::at(PATH.into(), files.clone())
    }

    fn route(remote: Option<&str>) -> Route {
        Route {
            target: "docker".into(),
            remote: remote.map(Into::into),
        }
    }

    fn project(path: &str) -> Place {
        Place::Project { path: path.into() }
    }

    fn names(history: &dyn History) -> Vec<String> {
        history
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn the_most_recent_place_comes_first_and_a_repeat_moves_up_instead_of_adding() {
        let history = Memory::default();
        history
            .remember(project("/a"), route(None), "a".into(), None)
            .unwrap();
        history
            .remember(project("/b"), route(None), "b".into(), None)
            .unwrap();
        history
            .remember(project("/a"), route(None), "a again".into(), None)
            .unwrap();
        assert_eq!(names(&history), ["a again", "b"]);
    }

    #[test]
    fn the_same_folder_through_another_remote_or_target_is_another_place() {
        let history = Memory::default();
        history
            .remember(project("/a"), route(None), "here".into(), None)
            .unwrap();
        history
            .remember(project("/a"), route(Some("me@box")), "there".into(), None)
            .unwrap();
        let other = Route {
            target: "podman".into(),
            remote: None,
        };
        history
            .remember(project("/a"), other, "podman".into(), None)
            .unwrap();
        assert_eq!(names(&history), ["podman", "there", "here"]);
    }

    #[test]
    fn what_a_container_was_attached_with_does_not_make_it_another_container() {
        let history = Memory::default();
        let attach = |workdir: Option<&str>| Place::Container {
            name: "db".into(),
            workdir: workdir.map(Into::into),
        };
        history
            .remember(attach(None), route(None), "db".into(), None)
            .unwrap();
        history
            .remember(attach(Some("/srv")), route(None), "db".into(), None)
            .unwrap();
        let entries = history.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].place, attach(Some("/srv")));
    }

    #[test]
    fn a_session_keeps_the_bundle_it_was_injected_from() {
        let history = Memory::default();
        history
            .remember(project("/a"), route(None), "a".into(), Some("x.tar".into()))
            .unwrap();
        history
            .remember(project("/b"), route(None), "b".into(), None)
            .unwrap();
        let bundles: Vec<_> = history
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.bundle)
            .collect();
        assert_eq!(bundles, [None, Some("x.tar".to_string())]);
    }

    #[test]
    fn an_entry_without_a_bundle_reads_as_no_bundle() {
        let files = MemoryFiles::new().with(
            PATH,
            r#"[{"place":{"kind":"project","path":"/a"},"route":{"target":"docker","remote":null},"name":"a","last_used":1}]"#,
        );
        let entries = kept_in(&files).entries().unwrap();
        assert_eq!(entries[0].bundle, None);
    }

    #[test]
    fn only_the_most_recent_places_are_kept() {
        let history = Memory::default();
        for i in 0..MAX_ENTRIES + 5 {
            history
                .remember(
                    project(&format!("/p{i}")),
                    route(None),
                    format!("p{i}"),
                    None,
                )
                .unwrap();
        }
        let kept = names(&history);
        assert_eq!(kept.len(), MAX_ENTRIES);
        assert_eq!(kept[0], format!("p{}", MAX_ENTRIES + 4));
        assert_eq!(kept.last().unwrap(), "p5");
    }

    #[test]
    fn a_place_is_named_by_its_folder_or_its_container() {
        assert_eq!(project("/home/me/api").fallback_name(), "api");
        assert_eq!(project("/").fallback_name(), "/");
        let container = Place::Container {
            name: "db".into(),
            workdir: None,
        };
        assert_eq!(container.fallback_name(), "db");
    }

    #[test]
    fn the_file_keeps_the_list_between_runs_and_starts_empty() {
        let files = MemoryFiles::new();
        assert!(kept_in(&files).entries().unwrap().is_empty());
        kept_in(&files)
            .remember(project("/a"), route(Some("me@box")), "a".into(), None)
            .unwrap();
        kept_in(&files)
            .remember(project("/b"), route(None), "b".into(), None)
            .unwrap();
        let entries = kept_in(&files).entries().unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(entries[1].route, route(Some("me@box")));
        assert!(entries[0].last_used > 0);
    }

    #[test]
    fn the_file_is_written_for_this_user_alone() {
        // How that is done is `Files`'; what is asked of it is private.
        let files = MemoryFiles::new();
        kept_in(&files)
            .remember(project("/a"), route(None), "a".into(), None)
            .unwrap();
        assert!(files.is_private(PATH));
    }

    #[test]
    fn a_file_that_is_not_a_history_is_named_and_replaced_by_the_next_session() {
        let files = MemoryFiles::new().with(PATH, "not json");
        let history = kept_in(&files);
        let err = format!("{:#}", history.entries().unwrap_err());
        assert!(
            err.contains("history.json") && err.contains("remove it"),
            "{err}"
        );
        history
            .remember(project("/a"), route(None), "a".into(), None)
            .unwrap();
        assert_eq!(names(&history), ["a"]);
    }

    #[test]
    fn a_disk_that_refuses_is_an_error_that_names_the_file_and_keeps_the_list() {
        let files = MemoryFiles::new();
        kept_in(&files)
            .remember(project("/a"), route(None), "a".into(), None)
            .unwrap();
        let files = files.refusing("disk full");
        let err = kept_in(&files)
            .remember(project("/b"), route(None), "b".into(), None)
            .unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("history.json") && said.contains("disk full"),
            "{said}"
        );
        assert_eq!(names(&kept_in(&files)), ["a"]);
    }

    #[test]
    fn a_system_without_a_data_directory_says_so() {
        let history = FileHistory {
            path: None,
            files: MemoryFiles::new(),
        };
        let err = history.entries().unwrap_err().to_string();
        assert!(err.contains("no data directory"), "{err}");
    }
}
