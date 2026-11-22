//! `n` on the first screen: finding a project that is not remembered yet, on this
//! machine or on another one.
//!
//! The person says where to look (this machine, a machine reached before, or one
//! typed in), and then chooses among that machine's projects exactly as they
//! would among local ones: [`browse::pick`] is the same, and only the host it is
//! handed differs. The host is not built here. It comes from `reach`, which the
//! composition root passes, so how a machine is reached stays its decision, and
//! the connection opened to look is the one the command then runs on.

use anyhow::Result;
use std::path::PathBuf;

use super::browse::{self, Files, Scope};
use super::prompt::{Ask, Choose};
use super::table::{Cell, Table, Tone};
use crate::devcontainer::CONFIG_LOCATIONS;
use crate::history::History;
use crate::host::{HostRef, Want};
use crate::output::warning;

/// A project, and the machine it is on.
pub struct Located {
    /// How the machine is reached, as `--remote` takes it; `None` for this one.
    pub remote: Option<String>,
    /// Already connected: the command runs on this very host.
    pub host: HostRef,
    pub dir: PathBuf,
}

/// Lets the person choose a project. `remote` is the machine named on the command
/// line, which settles where to look; without one the person is asked. `None`
/// when they leave.
pub fn pick(
    history: &dyn History,
    choose: &dyn Choose,
    ask: &dyn Ask,
    remote: Option<&str>,
    reach: &dyn Fn(Option<&str>) -> HostRef,
) -> Result<Option<Located>> {
    let (remote, host, home) = loop {
        let (remote, named) = match remote {
            Some(r) => (Some(r.to_string()), true),
            None => match where_to(history, choose, ask)? {
                Some(remote) => (remote, false),
                None => return Ok(None),
            },
        };
        match open(reach, remote.as_deref()) {
            Ok((host, home)) => break (remote, host, home),
            // A machine that was named is the answer to the command; one that was
            // chosen can be chosen again.
            Err(e) if !named => warning!("{e:#}"),
            Err(e) => return Err(e),
        }
    };
    let files = Files::new(&*host, &home);
    let scope = Scope::new(vec![home.clone()], Vec::new());
    let found = browse::pick(
        choose,
        "New project",
        &scope,
        Want::Project(CONFIG_LOCATIONS),
        &files,
    )?;
    let Some(dir) = found.or_ask(ask, "path of the project", &files)? else {
        return Ok(None);
    };
    Ok(Some(Located { remote, host, dir }))
}

/// Connects to `remote` (this machine for `None`) while the terminal is ours, and
/// finds its home directory.
fn open(
    reach: &dyn Fn(Option<&str>) -> HostRef,
    remote: Option<&str>,
) -> Result<(HostRef, PathBuf)> {
    let host = reach(remote);
    host.connect()?;
    let home = host.home_dir()?;
    Ok((host, home))
}

/// Which machine to look on: this one, one a session was opened on before (the
/// latest first), or another, typed. The outer `None` is leaving.
fn where_to(
    history: &dyn History,
    choose: &dyn Choose,
    ask: &dyn Ask,
) -> Result<Option<Option<String>>> {
    let mut known: Vec<String> = Vec::new();
    for entry in history.entries()? {
        if let Some(remote) = entry.route.remote
            && !known.contains(&remote)
        {
            known.push(remote);
        }
    }
    loop {
        let mut rows = vec![vec![
            Cell::new("this machine", Tone::Strong),
            Cell::new("", Tone::Muted),
        ]];
        rows.extend(known.iter().map(|r| {
            vec![
                Cell::new(r.clone(), Tone::Accent),
                Cell::new("over ssh", Tone::Muted),
            ]
        }));
        rows.push(vec![
            Cell::new("another machine…", Tone::Normal),
            Cell::new(
                "user@host, host:port or a name from ~/.ssh/config",
                Tone::Muted,
            ),
        ]);
        let options = Table::new(vec!["WHERE", ""], rows, 1);
        let Some(row) = choose.choose("Where is the project?", &options)? else {
            return Ok(None);
        };
        match row {
            0 => return Ok(Some(None)),
            n if n <= known.len() => return Ok(Some(Some(known[n - 1].clone()))),
            _ => {
                if let Some(typed) = ask.ask("ssh destination (the port is 22 unless given)")? {
                    return Ok(Some(Some(typed)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::prompt::{Item, Selection};
    use crate::history::{Entry, Memory, Place, Route};
    use crate::host::fake::FakeHost;
    use std::cell::RefCell;
    use std::sync::Arc;

    /// Answers the where-to question and the list, and keeps what it was shown.
    struct Script {
        row: RefCell<Vec<Option<usize>>>,
        shown: RefCell<Vec<Vec<String>>>,
        offered: RefCell<Vec<Vec<String>>>,
    }

    impl Script {
        fn rows(rows: Vec<Option<usize>>) -> Self {
            Script {
                row: RefCell::new(rows),
                shown: RefCell::default(),
                offered: RefCell::default(),
            }
        }
    }

    impl Choose for Script {
        fn choose(&self, _: &str, options: &Table) -> Result<Option<usize>> {
            self.offered.borrow_mut().push(options.natural());
            Ok(self.row.borrow_mut().remove(0))
        }
        fn find(&self, _: &str, _: &[&str], items: &[Item]) -> Result<Selection> {
            self.shown
                .borrow_mut()
                .push(items.iter().map(|i| i.name.clone()).collect());
            Ok(Selection::Items(vec![0]))
        }
    }

    struct Types(RefCell<Vec<Option<&'static str>>>);

    impl Ask for Types {
        fn ask(&self, _: &str) -> Result<Option<String>> {
            Ok(self.0.borrow_mut().remove(0).map(String::from))
        }
    }

    fn history(remotes: &[Option<&str>]) -> Memory {
        Memory::with(
            remotes
                .iter()
                .map(|r| Entry {
                    place: Place::Project { path: "/p".into() },
                    route: Route {
                        target: "docker".into(),
                        remote: r.map(Into::into),
                    },
                    name: "p".into(),
                    last_used: 0,
                    bundle: None,
                })
                .collect(),
        )
    }

    /// A machine that has one project, `~/work/api`.
    fn machine() -> HostRef {
        Arc::new(
            FakeHost::remote("me@box")
                .reply_once(&["sh", "-c"], crate::host::fake::Reply::ok("/home/me"))
                .on(
                    &["sh", "-c"],
                    "f/home/me/work/api/.devcontainer/devcontainer.json\0",
                ),
        )
    }

    #[test]
    fn a_project_is_chosen_on_the_machine_named_on_the_command_line() {
        let script = Script::rows(vec![]);
        let reach = |r: Option<&str>| {
            assert_eq!(r, Some("me@box"));
            machine()
        };
        let got = pick(
            &history(&[]),
            &script,
            &Types(RefCell::default()),
            Some("me@box"),
            &reach,
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.dir, PathBuf::from("/home/me/work/api"));
        assert_eq!(got.remote.as_deref(), Some("me@box"));
        assert_eq!(script.shown.borrow()[0], ["work/api"]);
    }

    #[test]
    fn without_a_machine_named_the_person_chooses_among_those_used_before_and_a_typed_one() {
        let known = history(&[Some("me@box"), None, Some("me@box"), Some("ci:2222")]);
        let script = Script::rows(vec![Some(2)]);
        let reach = |r: Option<&str>| {
            assert_eq!(r, Some("ci:2222"));
            machine()
        };
        let got = pick(&known, &script, &Types(RefCell::default()), None, &reach)
            .unwrap()
            .unwrap();
        assert_eq!(got.remote.as_deref(), Some("ci:2222"));
        let offered = &script.offered.borrow()[0];
        assert_eq!(offered.len(), 4, "this machine, two remembered, another");
        assert!(offered[0].starts_with("this machine"));
        assert!(offered[1].starts_with("me@box") && offered[2].starts_with("ci:2222"));

        let script = Script::rows(vec![Some(1)]);
        let reach = |r: Option<&str>| {
            assert_eq!(r, Some("new@host:2200"));
            machine()
        };
        let types = Types(RefCell::new(vec![Some("new@host:2200")]));
        let got = pick(&history(&[]), &script, &types, None, &reach)
            .unwrap()
            .unwrap();
        assert_eq!(got.remote.as_deref(), Some("new@host:2200"));
    }

    #[test]
    fn this_machine_is_reached_without_a_remote() {
        let script = Script::rows(vec![Some(0)]);
        let reach = |r: Option<&str>| {
            assert_eq!(r, None);
            machine()
        };
        let got = pick(
            &history(&[]),
            &script,
            &Types(RefCell::default()),
            None,
            &reach,
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.remote, None);
    }

    #[test]
    fn a_machine_that_cannot_be_reached_is_asked_for_again_unless_it_was_named() {
        let down = || -> HostRef {
            Arc::new(FakeHost::remote("down").reply_once(
                &["sh", "-c"],
                crate::host::fake::Reply::fail(255, "no route"),
            ))
        };
        // Chosen: back to the question, and leaving it ends the search.
        let script = Script::rows(vec![Some(1), None]);
        let types = Types(RefCell::new(vec![Some("down")]));
        let reach = |_: Option<&str>| down();
        assert!(
            pick(&history(&[]), &script, &types, None, &reach)
                .unwrap()
                .is_none()
        );
        assert_eq!(script.offered.borrow().len(), 2);

        // Named: the command fails, as `ldg --remote down up` would.
        let script = Script::rows(vec![]);
        let named = pick(
            &history(&[]),
            &script,
            &Types(RefCell::default()),
            Some("down"),
            &reach,
        );
        assert!(named.is_err());
    }

    #[test]
    fn leaving_the_question_or_the_list_finds_nothing() {
        let script = Script::rows(vec![None]);
        let reach = |_: Option<&str>| machine();
        let got = pick(
            &history(&[]),
            &script,
            &Types(RefCell::default()),
            None,
            &reach,
        );
        assert!(got.unwrap().is_none());
    }
}
