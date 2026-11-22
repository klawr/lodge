//! `ldg` on its own: choose among the places a session was opened in, and turn
//! the choice back into the command that opened it.
//!
//! Nothing here connects to anything. The choice only becomes a [`Command`] and
//! the [`Route`] to run it by; `run` then builds the host and the target from
//! those exactly as if they had been typed, so a resumed session and a typed one
//! cannot differ, and `ldg up` still wakes a stopped container.

use anyhow::Result;
use std::time::{Duration, UNIX_EPOCH};

use super::prompt::Choose;
use super::table::{Cell, Table, Tone};
use crate::cli::{Command, ProjectArg};
use crate::history::{Entry, History, Place, Route};
use crate::output::note;

/// What to run, and how to reach it.
pub struct Resumed {
    pub route: Route,
    pub command: Command,
}

/// Offers the remembered places, most recent first, and returns the one chosen:
/// `None` when there is nothing to offer or the person leaves. `remote` and
/// `target` are the flags given on the command line; when there are any, only
/// the places reached that way are offered.
pub fn pick(
    history: &dyn History,
    choose: &dyn Choose,
    remote: Option<&str>,
    target: Option<&str>,
) -> Result<Option<Resumed>> {
    let all = history.entries()?;
    let entries: Vec<Entry> = all
        .iter()
        .filter(|e| remote.is_none_or(|r| e.route.remote.as_deref() == Some(r)))
        .filter(|e| target.is_none_or(|t| e.route.target == t))
        .cloned()
        .collect();
    if entries.is_empty() {
        if all.is_empty() {
            note!(
                "nothing to resume yet: `ldg up` in a project, or `ldg attach NAME`, \
                 remembers where you were (`ldg --help` lists what else there is)"
            );
        } else {
            note!("none of the places you were in was reached that way");
        }
        return Ok(None);
    }
    let Some(chosen) = choose.choose("Where to?", &describe(&entries))? else {
        return Ok(None);
    };
    let entry = &entries[chosen];
    Ok(Some(Resumed {
        route: entry.route.clone(),
        command: command_for(&entry.place),
    }))
}

/// The command that opens `place` again: what was typed, without the flags that
/// only belong to one run (`--build`, `--reinject`, `--bundle`).
fn command_for(place: &Place) -> Command {
    match place {
        Place::Project { path } => Command::Up {
            project: ProjectArg {
                path: Some(path.clone()),
            },
            build: false,
            reinject: false,
            no_attach: false,
            bundle: None,
        },
        Place::Container { name, workdir } => Command::Attach {
            container: name.clone(),
            workdir: workdir.clone(),
            reinject: false,
            no_attach: false,
            bundle: None,
        },
    }
}

/// A row for each entry: its name, where it is, how it was reached and when it was
/// last used. The place is the column that takes the room there is, and gives way
/// when there is none; a place on another machine stands out.
fn describe(entries: &[Entry]) -> Table {
    let rows = entries
        .iter()
        .map(|e| {
            let place = match &e.place {
                Place::Project { path } => path.display().to_string(),
                Place::Container { name, .. } => format!("container {name}"),
            };
            let how = match &e.route.remote {
                Some(remote) => Cell::new(format!("{} on {remote}", e.route.target), Tone::Accent),
                None => Cell::new(e.route.target.clone(), Tone::Muted),
            };
            let when = UNIX_EPOCH + Duration::from_secs(e.last_used);
            vec![
                Cell::new(e.name.clone(), Tone::Strong),
                Cell::new(place, Tone::Normal),
                how,
                Cell::new(format!("{} ago", super::ago(when)), Tone::Muted),
            ]
        })
        .collect();
    Table::new(vec!["NAME", "WHERE", "VIA", "LAST"], rows, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Memory;
    use std::cell::RefCell;
    use std::time::SystemTime;

    /// Answers with a fixed choice and keeps what it was asked.
    struct Answers(Option<usize>, RefCell<Vec<Vec<String>>>);

    impl Answers {
        fn with(choice: Option<usize>) -> Self {
            Answers(choice, RefCell::default())
        }

        fn offered(&self) -> Vec<Vec<String>> {
            self.1.borrow().clone()
        }
    }

    impl Choose for Answers {
        fn choose(&self, _: &str, options: &Table) -> Result<Option<usize>> {
            self.1.borrow_mut().push(options.natural());
            Ok(self.0)
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn entry(place: Place, target: &str, remote: Option<&str>, name: &str, age: u64) -> Entry {
        Entry {
            place,
            route: Route {
                target: target.into(),
                remote: remote.map(Into::into),
            },
            name: name.into(),
            last_used: now() - age,
        }
    }

    fn project(path: &str) -> Place {
        Place::Project { path: path.into() }
    }

    fn history() -> Memory {
        Memory::with(vec![
            entry(project("/home/me/api"), "docker", None, "Demo API", 7200),
            entry(
                Place::Container {
                    name: "db".into(),
                    workdir: Some("/srv".into()),
                },
                "podman",
                Some("me@box"),
                "db",
                90_000,
            ),
            entry(project("/work/web"), "docker", Some("me@box"), "web", 120),
        ])
    }

    #[test]
    fn each_place_is_offered_with_its_name_its_route_and_how_long_ago() {
        let answers = Answers::with(None);
        assert!(pick(&history(), &answers, None, None).unwrap().is_none());
        let offered = &answers.offered()[0];
        assert_eq!(offered.len(), 3);
        assert!(
            offered[0].starts_with("Demo API  /home/me/api") && offered[0].ends_with("2 h ago"),
            "{offered:?}"
        );
        assert!(
            offered[1].contains("container db") && offered[1].contains("podman on me@box"),
            "{offered:?}"
        );
        assert!(offered[1].ends_with("1 d ago"), "{offered:?}");
        // Columns line up, whatever the names are.
        let column = |line: &str| line.find("docker").or_else(|| line.find("podman"));
        assert_eq!(column(&offered[0]), column(&offered[1]));
    }

    #[test]
    fn a_project_is_resumed_as_up_on_the_route_it_was_reached_by() {
        let resumed = pick(&history(), &Answers::with(Some(2)), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.route.remote.as_deref(), Some("me@box"));
        assert_eq!(resumed.route.target, "docker");
        let Command::Up {
            project,
            build,
            reinject,
            no_attach,
            bundle,
        } = resumed.command
        else {
            panic!("not up");
        };
        assert_eq!(
            project.path.as_deref(),
            Some(std::path::Path::new("/work/web"))
        );
        assert!(!build && !reinject && !no_attach && bundle.is_none());
    }

    #[test]
    fn a_container_is_resumed_as_attach_with_the_workdir_it_had() {
        let resumed = pick(&history(), &Answers::with(Some(1)), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.route.target, "podman");
        let Command::Attach {
            container, workdir, ..
        } = resumed.command
        else {
            panic!("not attach");
        };
        assert_eq!(container, "db");
        assert_eq!(workdir.as_deref(), Some("/srv"));
    }

    #[test]
    fn flags_on_the_command_line_narrow_what_is_offered() {
        let answers = Answers::with(Some(0));
        let resumed = pick(&history(), &answers, Some("me@box"), None)
            .unwrap()
            .unwrap();
        assert_eq!(answers.offered()[0].len(), 2);
        // The first of those two is the podman container, not the first of all.
        assert_eq!(resumed.route.target, "podman");
        let answers = Answers::with(Some(0));
        pick(&history(), &answers, Some("me@box"), Some("docker")).unwrap();
        assert_eq!(answers.offered()[0].len(), 1);
    }

    #[test]
    fn with_nothing_to_offer_nobody_is_asked() {
        let answers = Answers::with(Some(0));
        assert!(
            pick(&Memory::default(), &answers, None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            pick(&history(), &answers, Some("elsewhere"), None)
                .unwrap()
                .is_none()
        );
        assert!(answers.offered().is_empty());
    }

    #[test]
    fn leaving_resumes_nothing() {
        assert!(
            pick(&history(), &Answers::with(None), None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn when_there_is_nobody_to_ask_that_is_the_error() {
        struct Nobody;
        impl Choose for Nobody {
            fn choose(&self, question: &str, _: &Table) -> Result<Option<usize>> {
                anyhow::bail!("cannot ask \"{question}\" without a terminal")
            }
        }
        let err = pick(&history(), &Nobody, None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("without a terminal"), "{err}");
    }
}
