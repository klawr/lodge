//! `ldg` on its own: choose among the places a session was opened in, and turn
//! the choice back into the command that opened it.
//!
//! Nothing here connects to anything. The choice only becomes a [`Command`] and
//! the [`Route`] to run it by; `run` then builds the host and the target from
//! those exactly as if they had been typed, so a resumed session and a typed one
//! cannot differ, and `ldg up` still wakes a stopped container. `r` and `b` add
//! the one flag they stand for (`--reinject`, `--build`), so what is asked for
//! once is not remembered for next time.

use anyhow::Result;
use std::time::{Duration, UNIX_EPOCH};

use super::prompt::{Choice, Choose, Hotkey};
use super::table::{Cell, Table, Tone};
use crate::cli::{Command, ProjectArg};
use crate::history::{Entry, History, Place, Route};
use crate::output::note;

/// What the person chose on the first screen.
pub enum Home {
    /// A place to go back to.
    Place(Resumed),
    /// To look at and change what is injected.
    Configure,
}

/// What to run, and how to reach it.
pub struct Resumed {
    pub route: Route,
    pub command: Command,
}

/// The keys besides Enter: the config, and the two ways to open a place again
/// that Enter does not do.
const KEYS: &[Hotkey] = &[
    Hotkey {
        key: 'c',
        label: "config",
    },
    Hotkey {
        key: 'r',
        label: "reinject",
    },
    Hotkey {
        key: 'b',
        label: "recreate",
    },
];

/// Offers the remembered places, most recent first, with `c` for the config,
/// `r` to open one and inject again, and `b` to recreate its container, and
/// returns what was chosen: `None` when the person leaves. `remote` and
/// `target` are the flags given on the command line; when there are any, only
/// the places reached that way are offered.
pub fn pick(
    history: &dyn History,
    choose: &dyn Choose,
    remote: Option<&str>,
    target: Option<&str>,
) -> Result<Option<Home>> {
    let all = history.entries()?;
    let entries: Vec<Entry> = all
        .iter()
        .filter(|e| remote.is_none_or(|r| e.route.remote.as_deref() == Some(r)))
        .filter(|e| target.is_none_or(|t| e.route.target == t))
        .cloned()
        .collect();
    loop {
        let (key, row) = match choose.choose_with("Where to?", &describe(&entries), KEYS)? {
            Choice::Left => return Ok(None),
            Choice::Key('c', _) => return Ok(Some(Home::Configure)),
            Choice::Key(key, row) => (Some(key), row),
            Choice::Row(row) => (None, row),
        };
        // With nothing remembered the one row says so, and means nothing.
        let Some(entry) = entries.get(row) else {
            continue;
        };
        let (reinject, build) = (key == Some('r'), key == Some('b'));
        if build {
            if !matches!(entry.place, Place::Project { .. }) {
                note!(
                    "{} is a container ldg did not make: it can be reinjected (r), not recreated",
                    entry.name
                );
                continue;
            }
            if !confirm_recreate(choose, &entry.name)? {
                continue;
            }
        }
        return Ok(Some(Home::Place(Resumed {
            route: entry.route.clone(),
            command: command_for(&entry.place, reinject, build),
        })));
    }
}

/// Recreating throws away the container and what was installed in it, so it is
/// asked once more.
fn confirm_recreate(choose: &dyn Choose, name: &str) -> Result<bool> {
    let options = Table::new(
        vec!["DO"],
        vec![
            vec![Cell::new("recreate it", Tone::Strong)],
            vec![Cell::new("cancel", Tone::Muted)],
        ],
        0,
    );
    let question =
        format!("Recreate {name}? Its container and what is installed in it are removed");
    Ok(choose.choose(&question, &options)? == Some(0))
}

/// The command that opens `place` again: what was typed, without the flags that
/// only belong to one run (`--bundle`), and with the one that was asked for:
/// `--reinject` for a project or a container, `--build` for a project.
fn command_for(place: &Place, reinject: bool, build: bool) -> Command {
    match place {
        Place::Project { path } => Command::Up {
            project: ProjectArg {
                path: Some(path.clone()),
            },
            build,
            reinject,
            no_attach: false,
            bundle: None,
        },
        Place::Container { name, workdir } => Command::Attach {
            container: name.clone(),
            workdir: workdir.clone(),
            reinject,
            no_attach: false,
            bundle: None,
        },
    }
}

/// A row for each entry: its name, where it is, how it was reached and when it was
/// last used. The place is the column that takes the room there is, and gives way
/// when there is none; a place on another machine stands out.
fn describe(entries: &[Entry]) -> Table {
    let mut rows: Vec<Vec<Cell>> = entries
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
    if rows.is_empty() {
        let none = |text: &str| Cell::new(text, Tone::Muted);
        rows.push(vec![
            none("no places yet"),
            none("`ldg up` in a project or `ldg attach NAME` remembers one; c for the config"),
            none(""),
            none(""),
        ]);
    }
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

    /// Presses the config key.
    struct PressesC;
    impl Choose for PressesC {
        fn choose(&self, _: &str, _: &Table) -> Result<Option<usize>> {
            Ok(None)
        }
        fn choose_with(&self, _: &str, _: &Table, keys: &[Hotkey]) -> Result<Choice> {
            assert_eq!(
                keys.iter().map(|k| k.key).collect::<Vec<_>>(),
                vec!['c', 'r', 'b']
            );
            Ok(Choice::Key('c', 0))
        }
    }

    impl Home {
        fn place(self) -> Resumed {
            match self {
                Home::Place(p) => p,
                Home::Configure => panic!("not a place"),
            }
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
            .unwrap()
            .place();
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
            .unwrap()
            .place();
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
            .unwrap()
            .place();
        assert_eq!(answers.offered()[0].len(), 2);
        // The first of those two is the podman container, not the first of all.
        assert_eq!(resumed.route.target, "podman");
        let answers = Answers::with(Some(0));
        pick(&history(), &answers, Some("me@box"), Some("docker")).unwrap();
        assert_eq!(answers.offered()[0].len(), 1);
    }

    #[test]
    fn the_config_key_leads_to_the_config_with_places_or_without() {
        let home = pick(&history(), &PressesC, None, None).unwrap().unwrap();
        assert!(matches!(home, Home::Configure));
        let home = pick(&Memory::default(), &PressesC, None, None)
            .unwrap()
            .unwrap();
        assert!(matches!(home, Home::Configure));
    }

    #[test]
    fn with_no_places_one_row_says_so_and_choosing_it_asks_again() {
        // The row means nothing, so a choice of it is asked again, then left.
        struct Rows(RefCell<Vec<Option<usize>>>);
        impl Choose for Rows {
            fn choose(&self, _: &str, options: &Table) -> Result<Option<usize>> {
                assert_eq!(options.len(), 1);
                Ok(self.0.borrow_mut().remove(0))
            }
        }
        let rows = Rows(RefCell::new(vec![Some(0), None]));
        assert!(
            pick(&Memory::default(), &rows, None, None)
                .unwrap()
                .is_none()
        );
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

    /// Presses `key` on a row, then answers the confirmation (if there is one).
    struct Presses {
        key: char,
        row: usize,
        confirm: Option<usize>,
        asked: RefCell<Vec<String>>,
        /// How often the list was shown: the second time the person leaves.
        shown: std::cell::Cell<u32>,
    }
    impl Choose for Presses {
        fn choose(&self, question: &str, _: &Table) -> Result<Option<usize>> {
            self.asked.borrow_mut().push(question.to_string());
            Ok(self.confirm)
        }
        fn choose_with(&self, _: &str, _: &Table, _: &[Hotkey]) -> Result<Choice> {
            self.shown.set(self.shown.get() + 1);
            Ok(if self.shown.get() == 1 {
                Choice::Key(self.key, self.row)
            } else {
                Choice::Left
            })
        }
    }

    fn presses(key: char, row: usize, confirm: Option<usize>) -> Presses {
        Presses {
            key,
            row,
            confirm,
            asked: RefCell::default(),
            shown: Default::default(),
        }
    }

    #[test]
    fn r_opens_the_place_again_with_reinject_and_nothing_else_asked_for() {
        // Row 2 is the project, row 1 the container: both can be reinjected.
        let project = pick(&history(), &presses('r', 2, None), None, None)
            .unwrap()
            .unwrap()
            .place();
        let Command::Up {
            build, reinject, ..
        } = project.command
        else {
            panic!("not up");
        };
        assert!(reinject && !build);
        let container = pick(&history(), &presses('r', 1, None), None, None)
            .unwrap()
            .unwrap()
            .place();
        let Command::Attach { reinject, .. } = container.command else {
            panic!("not attach");
        };
        assert!(reinject);
        // The route is the one it was reached by, as with Enter.
        assert_eq!(project.route.remote.as_deref(), Some("me@box"));
    }

    #[test]
    fn b_recreates_a_project_after_asking_and_cancelling_asks_for_the_list_again() {
        let yes = presses('b', 2, Some(0));
        let recreated = pick(&history(), &yes, None, None).unwrap().unwrap().place();
        let Command::Up {
            build, reinject, ..
        } = recreated.command
        else {
            panic!("not up");
        };
        assert!(build && !reinject);
        assert!(
            yes.asked.borrow()[0].starts_with("Recreate web?"),
            "{:?}",
            yes.asked
        );
        // Cancelled: nothing is recreated, and the list is shown again (and left).
        let no = presses('b', 2, Some(1));
        assert!(pick(&history(), &no, None, None).unwrap().is_none());
        assert_eq!(no.asked.borrow().len(), 1);
    }

    #[test]
    fn a_container_that_ldg_did_not_make_is_not_recreated_and_nobody_is_asked() {
        let container = presses('b', 1, Some(0));
        assert!(pick(&history(), &container, None, None).unwrap().is_none());
        assert!(container.asked.borrow().is_empty());
        // With nothing remembered the keys mean nothing either.
        let none = presses('r', 0, None);
        assert!(
            pick(&Memory::default(), &none, None, None)
                .unwrap()
                .is_none()
        );
    }
}
