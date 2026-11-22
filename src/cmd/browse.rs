//! Finding a file or a folder by typing part of its name, for the places that
//! used to ask for a path.
//!
//! One walk and one list serve every question: the config screen asks for a file
//! (the config to edit) or for what to inject, and the first screen asks for a
//! project that is not remembered yet. What differs is [`Want`], what counts as
//! an answer, and the [`Scope`], where to look. The list is the finder of
//! `select`, reached through [`Choose::find`], so a test answers it like any
//! other question.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::prompt::{Ask, Choose, Item, Knob, Selection};
use crate::host::{Entry, Host, Kind, Search, Want};
use crate::keys::Action;
use crate::layout::tilde;

fn noun(want: Want) -> &'static str {
    match want {
        Want::File => "files",
        Want::Any => "files and folders",
        Want::Project(_) => "projects",
    }
}

/// The files of one machine as a search sees them: the host to ask, and its home
/// directory, which `~` means and relative paths start from. A search of another
/// machine and of this one differ only in which host is handed in.
pub struct Files<'a> {
    host: &'a dyn Host,
    home: &'a Path,
}

impl<'a> Files<'a> {
    pub fn new(host: &'a dyn Host, home: &'a Path) -> Self {
        Files { host, home }
    }

    pub fn home(&self) -> &Path {
        self.home
    }

    /// What `scope` reaches that is wanted, nearest first.
    pub fn walk(&self, scope: &Scope, want: Want) -> Result<Vec<Entry>> {
        self.host.find(&scope.search(want))
    }

    fn inspect(&self, path: &Path, want: Want) -> Result<Kind> {
        self.host.inspect(path, want.markers())
    }

    /// A path as it was typed, made absolute on the machine.
    pub fn resolve(&self, typed: &str) -> PathBuf {
        self.host.resolve(typed, self.home)
    }
}

/// Folders by name, at any depth, that a search never enters or lists: they
/// hold what tools keep, not what a person looks for, and are the bulk of what
/// a walk would meet.
const DENIED_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "build",
    "tmp",
    "cache",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".nvm",
    ".venv",
    "__pycache__",
    ".Trash",
];

/// Where a search looks and what it leaves out: a whitelist of roots, then a
/// blacklist of names and paths. No roots is no whitelist, which is `/`. The
/// roots and the names can be edited from inside the search (see [`Choose::find_knobs`]); the
/// paths, which are credentials, cannot. The blacklist is only for walking: a
/// path typed in full is never refused by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    roots: Vec<PathBuf>,
    /// The roots it started with, for going back to them.
    default_roots: Vec<PathBuf>,
    denied_names: Vec<String>,
    denied_paths: Vec<PathBuf>,
}

/// The knobs of a search, by index.
const ROOTS: usize = 0;
const NAMES: usize = 1;
const EVERYWHERE: usize = 2;

impl Scope {
    /// Looks under `roots`, leaving out the usual folders and the system's, and
    /// `denied` besides (what must not be offered, such as credentials).
    pub fn new(roots: Vec<PathBuf>, denied: Vec<PathBuf>) -> Self {
        let mut denied_paths = vec!["/proc".into(), "/sys".into(), "/dev".into()];
        denied_paths.extend(denied);
        Scope {
            default_roots: roots.clone(),
            roots,
            denied_names: DENIED_NAMES.iter().map(|n| n.to_string()).collect(),
            denied_paths,
        }
    }

    /// The same blacklist, looking under `root` alone.
    pub fn at(&self, root: PathBuf) -> Self {
        Scope {
            roots: vec![root],
            ..self.clone()
        }
    }

    /// What to ask a host for to look for `want`.
    fn search(&self, want: Want) -> Search {
        Search {
            roots: self.roots(),
            denied_names: self.denied_names.clone(),
            denied_paths: self.denied_paths.clone(),
            want,
        }
    }

    fn roots(&self) -> Vec<PathBuf> {
        if self.roots.is_empty() {
            vec![PathBuf::from("/")]
        } else {
            self.roots.clone()
        }
    }

    /// The settings of the search that can be edited.
    pub fn knobs(&self, home: &Path) -> Vec<Knob> {
        let show =
            |roots: &[PathBuf]| -> Vec<String> { roots.iter().map(|r| tilde(r, home)).collect() };
        let everywhere = self.is_everywhere();
        let back = if self.default_roots.is_empty() {
            "/".to_string()
        } else {
            show(&self.default_roots).join(" ")
        };
        vec![
            Knob {
                key: Action::LookWhere,
                name: "looking in",
                action: "change where to look".into(),
                text: show(&self.roots()).join(" "),
                editable: true,
                changed: self.roots() != self.default_roots_or_all(),
            },
            Knob {
                key: Action::Skip,
                name: "skipping",
                action: "change what is skipped".into(),
                text: self.denied_names.join(" "),
                editable: true,
                changed: self.denied_names != DENIED_NAMES,
            },
            Knob {
                key: Action::Everywhere,
                name: "everywhere",
                action: if everywhere {
                    format!("look only in {back}")
                } else {
                    "look everywhere".into()
                },
                text: String::new(),
                editable: false,
                changed: false,
            },
        ]
    }

    fn is_everywhere(&self) -> bool {
        self.roots.is_empty() || self.roots == [PathBuf::from("/")]
    }

    /// Where it began, as [`Scope::roots`] gives roots: no roots is `/`.
    fn default_roots_or_all(&self) -> Vec<PathBuf> {
        if self.default_roots.is_empty() {
            vec![PathBuf::from("/")]
        } else {
            self.default_roots.clone()
        }
    }

    /// This scope with knob `knob` set to `text`: the roots or the names are
    /// the words of it, and everywhere switches between `/` and where it began.
    pub fn edited(&self, knob: usize, text: &str, files: &Files) -> Self {
        let mut next = self.clone();
        match knob {
            ROOTS => {
                next.roots = text.split_whitespace().map(|r| files.resolve(r)).collect();
            }
            NAMES => {
                next.denied_names = text.split_whitespace().map(str::to_string).collect();
            }
            EVERYWHERE => {
                next.roots = if self.is_everywhere() {
                    self.default_roots.clone()
                } else {
                    Vec::new()
                };
            }
            _ => {}
        }
        next
    }
}

/// How a search for a path ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Browsed {
    Chose(PathBuf),
    Left,
    /// The terminal can't show a list: ask for the path in words instead.
    Unsupported,
}

impl Browsed {
    /// The path chosen; `None` when the person left. A terminal that can't show a
    /// list is asked `question` in words.
    pub fn or_ask(self, ask: &dyn Ask, question: &str, files: &Files) -> Result<Option<PathBuf>> {
        Ok(match self {
            Browsed::Chose(path) => Some(path),
            Browsed::Left => None,
            Browsed::Unsupported => ask.ask(question)?.map(|typed| files.resolve(&typed)),
        })
    }
}

/// Lets the person find something of `want` in `scope`, by typing part of its
/// path. Where it looks and what it skips are edited from inside the list, and
/// the list is made again; a path typed in full is settled by [`settle`].
pub fn pick(
    choose: &dyn Choose,
    title: &str,
    scope: &Scope,
    want: Want,
    files: &Files,
) -> Result<Browsed> {
    let mut scope = scope.clone();
    loop {
        let found = files.walk(&scope, want)?;
        let items = items(&found, &scope, want, files.home());
        let heading = format!("{title} — {}", noun(want));
        match choose.find_knobs(&heading, &[noun(want)], &items, &scope.knobs(files.home()))? {
            Selection::Left => return Ok(Browsed::Left),
            Selection::Unsupported => return Ok(Browsed::Unsupported),
            Selection::Edited(knob, text) => scope = scope.edited(knob, &text, files),
            Selection::Items(chosen) => {
                return Ok(match chosen.first().and_then(|&i| found.get(i)) {
                    Some(e) => Browsed::Chose(e.path.clone()),
                    None => Browsed::Left,
                });
            }
            Selection::Path(typed) => return settle(choose, title, &typed, want, &scope, files),
        }
    }
}

/// The paths found, as the rows of a list. Under one root they start there;
/// under several, where they are.
pub fn items(found: &[Entry], scope: &Scope, want: Want, home: &Path) -> Vec<Item> {
    let only = match scope.roots.as_slice() {
        [root] => Some(root.as_path()),
        _ => None,
    };
    found
        .iter()
        .map(|e| Item {
            added: false,
            tab: 0,
            name: match only {
                Some(root) => e
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&e.path)
                    .display()
                    .to_string(),
                None => tilde(&e.path, home),
            },
            kind: match (want, e.dir) {
                (Want::Project(_), _) => "project",
                (_, true) => "folder",
                (_, false) => "file",
            },
            detail: String::new(),
        })
        .collect()
}

/// What a path typed in full means: itself, or, when it is a folder that is no
/// answer (a file is wanted, a project is, and this has no devcontainer config)
/// or one that was typed with a `/` at the end, the place to search in. That is
/// how the whitelist is changed: `~/work/` looks there, `/` looks everywhere.
pub fn settle(
    choose: &dyn Choose,
    title: &str,
    typed: &str,
    want: Want,
    scope: &Scope,
    files: &Files,
) -> Result<Browsed> {
    let path = files.resolve(typed);
    let kind = files.inspect(&path, want)?;
    let search = kind.is_folder()
        && match want {
            Want::File => true,
            Want::Any => typed.ends_with('/'),
            Want::Project(_) => kind != Kind::Project,
        };
    if search {
        pick(choose, title, &scope.at(path), want, files)
    } else {
        Ok(Browsed::Chose(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;
    use crate::tmpfile::TempDir;
    use std::cell::RefCell;

    /// Answers a search with what it is told and keeps the lists it was shown.
    struct Finds(RefCell<Vec<Selection>>, RefCell<Vec<Vec<String>>>);

    impl Finds {
        fn answering(answers: Vec<Selection>) -> Self {
            Finds(RefCell::new(answers), RefCell::default())
        }
        fn shown(&self) -> Vec<Vec<String>> {
            self.1.borrow().clone()
        }
    }

    impl Choose for Finds {
        fn choose(&self, _: &str, _: &crate::cmd::table::Table) -> Result<Option<usize>> {
            Ok(None)
        }
        fn find(&self, _: &str, _: &[&str], items: &[Item]) -> Result<Selection> {
            self.1
                .borrow_mut()
                .push(items.iter().map(|i| i.name.clone()).collect());
            Ok(self.0.borrow_mut().remove(0))
        }
    }

    fn tree() -> TempDir {
        let dir = TempDir::new("browse").unwrap();
        for file in [
            "top.txt",
            "a/one.txt",
            "a/b/two.txt",
            "node_modules/x/y.js",
            "build/out.o",
            ".git/HEAD",
            ".ssh/id_rsa",
            "proj/.devcontainer/devcontainer.json",
            "proj/src/main.rs",
            ".hidden/.devcontainer.json",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").unwrap();
        }
        dir
    }

    /// Looks under `root`, with `.ssh` there withheld as credentials are.
    fn scope(root: &Path) -> Scope {
        Scope::new(vec![root.to_path_buf()], vec![root.join(".ssh")])
    }

    fn names(root: &Path, want: Want) -> Vec<String> {
        walk(&scope(root), want)
            .iter()
            .map(|p| slashed(&p.strip_prefix(root).unwrap().display().to_string()))
            .collect()
    }

    /// A path as a list shows it, with `/` whatever the platform joins with.
    fn slashed(path: &str) -> String {
        path.replace('\\', "/")
    }

    const PROJECT: Want = Want::Project(crate::devcontainer::CONFIG_LOCATIONS);

    const HOME: &str = "/nowhere";

    fn files() -> Files<'static> {
        Files::new(&LocalHost, Path::new(HOME))
    }

    fn walk(scope: &Scope, want: Want) -> Vec<PathBuf> {
        let found = files().walk(scope, want).unwrap();
        found.into_iter().map(|e| e.path).collect()
    }

    #[test]
    fn a_walk_lists_files_and_folders_nearest_first() {
        let dir = tree();
        let files = names(dir.path(), Want::File);
        assert_eq!(files[..1], ["top.txt"], "what is nearest comes first");
        assert!(files.contains(&"a/b/two.txt".to_string()));
        let any = names(dir.path(), Want::Any);
        assert!(
            any.contains(&"a/b".to_string()),
            "folders come with the files"
        );
    }

    #[test]
    fn the_blacklist_is_by_name_anywhere_and_by_path_for_what_is_withheld() {
        let dir = tree();
        let any = names(dir.path(), Want::Any);
        for hidden in ["node_modules", "build", ".git", ".ssh"] {
            assert!(
                !any.iter().any(|f| f.starts_with(hidden)),
                "{hidden} is left out, and so is what is in it"
            );
        }
        assert!(
            any.contains(&".hidden".to_string()),
            "other dotfolders are there"
        );
    }

    #[test]
    fn no_roots_looks_everywhere_but_the_blacklist_still_holds() {
        let dir = tree();
        let everywhere = Scope::new(vec![], vec![dir.path().join(".ssh")]);
        assert_eq!(everywhere.roots(), [PathBuf::from("/")]);
        let narrowed = everywhere.at(dir.path().to_path_buf());
        let found = walk(&narrowed, Want::File);
        assert!(found.contains(&dir.path().join("top.txt")));
        assert!(!found.contains(&dir.path().join(".ssh/id_rsa")));
    }

    #[test]
    fn several_roots_are_walked_in_turn_without_listing_a_path_twice() {
        let dir = tree();
        let both = Scope::new(vec![dir.path().join("a"), dir.path().join("a/b")], vec![]);
        let found = walk(&both, Want::File);
        let two = dir.path().join("a/b/two.txt");
        assert_eq!(found.iter().filter(|p| **p == two).count(), 1);
    }

    #[test]
    fn a_project_is_a_folder_with_a_devcontainer_config_and_not_a_hidden_one() {
        let dir = tree();
        assert_eq!(names(dir.path(), PROJECT), ["proj"]);
    }

    #[test]
    fn a_choice_from_the_list_is_the_path_under_the_root() {
        let dir = tree();
        let ask = Finds::answering(vec![Selection::Items(vec![0])]);
        let got = pick(&ask, "File", &scope(dir.path()), Want::File, &files()).unwrap();
        assert_eq!(got, Browsed::Chose(dir.path().join("top.txt")));
        assert_eq!(
            ask.shown()[0][0],
            "top.txt",
            "shown relative to where it looks"
        );
    }

    #[test]
    fn a_typed_folder_is_searched_when_a_file_is_wanted_and_a_typed_file_is_taken() {
        let dir = tree();
        let a = dir.path().join("a");
        let ask = Finds::answering(vec![Selection::Items(vec![1])]);
        let s = scope(dir.path());
        let got = settle(&ask, "File", a.to_str().unwrap(), Want::File, &s, &files()).unwrap();
        assert_eq!(got, Browsed::Chose(a.join("b/two.txt")));
        assert_eq!(
            ask.shown()[0]
                .iter()
                .map(|s| slashed(s))
                .collect::<Vec<_>>(),
            ["one.txt", "b/two.txt"]
        );

        let none = Finds::answering(vec![]);
        let file = a.join("one.txt");
        let got = settle(
            &none,
            "File",
            file.to_str().unwrap(),
            Want::File,
            &s,
            &files(),
        )
        .unwrap();
        assert_eq!(got, Browsed::Chose(file));
    }

    #[test]
    fn a_typed_folder_is_taken_as_it_is_unless_it_ends_in_a_slash() {
        let dir = tree();
        let a = dir.path().join("a");
        let s = scope(dir.path());
        let none = Finds::answering(vec![]);
        let got = settle(&none, "Data", a.to_str().unwrap(), Want::Any, &s, &files()).unwrap();
        assert_eq!(got, Browsed::Chose(a.clone()));

        let ask = Finds::answering(vec![Selection::Left]);
        let slash = format!("{}/", a.display());
        let got = settle(&ask, "Data", &slash, Want::Any, &s, &files()).unwrap();
        assert_eq!(got, Browsed::Left);
    }

    #[test]
    fn a_typed_path_is_never_refused_by_the_blacklist() {
        let dir = tree();
        let none = Finds::answering(vec![]);
        let key = dir.path().join(".ssh/id_rsa");
        let s = scope(dir.path());
        let got = settle(
            &none,
            "File",
            key.to_str().unwrap(),
            Want::File,
            &s,
            &files(),
        );
        assert_eq!(got.unwrap(), Browsed::Chose(key));
    }

    #[test]
    fn a_typed_project_is_taken_and_a_typed_folder_without_one_is_searched() {
        let dir = tree();
        let proj = dir.path().join("proj");
        let s = scope(dir.path());
        let none = Finds::answering(vec![]);
        let got = settle(
            &none,
            "Project",
            proj.to_str().unwrap(),
            PROJECT,
            &s,
            &files(),
        );
        assert_eq!(got.unwrap(), Browsed::Chose(proj.clone()));

        let ask = Finds::answering(vec![Selection::Items(vec![0])]);
        let top = dir.path().to_str().unwrap();
        let got = settle(&ask, "Project", top, PROJECT, &s, &files()).unwrap();
        assert_eq!(got, Browsed::Chose(proj));
    }

    #[test]
    fn the_roots_are_edited_as_words_and_empty_is_everywhere() {
        let dir = tree();
        let a = dir.path().join("a");
        let both = format!("{} {}", a.display(), dir.path().join("proj").display());
        let s = scope(dir.path()).edited(ROOTS, &both, &files());
        assert_eq!(s.roots, [a, dir.path().join("proj")]);
        let s = s.edited(ROOTS, "", &files());
        assert_eq!(s.roots(), [PathBuf::from("/")]);
        assert_eq!(s.knobs(Path::new(HOME))[ROOTS].text, "/");
    }

    #[test]
    fn everywhere_switches_between_the_root_and_where_the_search_began() {
        let dir = tree();
        let s = scope(dir.path());
        let wide = s.edited(EVERYWHERE, "", &files());
        assert_eq!(wide.roots(), [PathBuf::from("/")]);
        assert_eq!(wide.edited(EVERYWHERE, "", &files()).roots, s.roots);
    }

    #[test]
    fn the_names_skipped_are_edited_and_what_is_let_through_is_found() {
        let dir = tree();
        let s = scope(dir.path()).edited(NAMES, ".git node_modules", &files());
        let any = walk(&s, Want::File);
        assert!(
            any.contains(&dir.path().join("build/out.o")),
            "build is let through"
        );
        assert!(!any.contains(&dir.path().join("node_modules/x/y.js")));
        assert!(
            !any.contains(&dir.path().join(".ssh/id_rsa")),
            "credentials are not a name"
        );
    }

    #[test]
    fn an_edit_in_the_list_makes_the_list_again_from_the_new_scope() {
        let dir = tree();
        let a = dir.path().join("a");
        let ask = Finds::answering(vec![
            Selection::Edited(ROOTS, a.display().to_string()),
            Selection::Items(vec![0]),
        ]);
        let got = pick(&ask, "File", &scope(dir.path()), Want::File, &files()).unwrap();
        assert_eq!(got, Browsed::Chose(a.join("one.txt")));
        let shown = ask.shown();
        assert!(shown[0].contains(&"top.txt".to_string()));
        assert_eq!(
            shown[1].iter().map(|s| slashed(s)).collect::<Vec<_>>(),
            ["one.txt", "b/two.txt"],
            "now under the new root"
        );
    }

    #[test]
    fn the_knobs_say_how_the_scope_stands_and_what_is_other_than_it_began() {
        let dir = tree();
        let s = scope(dir.path());
        let k = s.knobs(Path::new(HOME));
        assert!(k.iter().all(|k| !k.changed));
        assert_eq!(k[EVERYWHERE].action, "look everywhere");

        let wide = s.edited(EVERYWHERE, "", &files());
        let k = wide.knobs(Path::new(HOME));
        assert!(k[ROOTS].changed && k[ROOTS].text == "/");
        assert!(
            k[EVERYWHERE].action.starts_with("look only in"),
            "{:?}",
            k[EVERYWHERE]
        );
        assert!(!k[NAMES].changed);

        let skipping = s.edited(NAMES, ".git", &files());
        let k = skipping.knobs(Path::new(HOME));
        assert!(k[NAMES].changed && !k[ROOTS].changed);
    }

    /// A host that only runs commands (locally), so its search is the script.
    #[cfg(unix)]
    struct Sh;

    #[cfg(unix)]
    impl Host for Sh {
        fn label(&self) -> Option<String> {
            Some("sh".into())
        }
        fn names(&self) -> Vec<String> {
            vec!["sh".into()]
        }
        fn shares_cwd(&self) -> bool {
            false
        }
        fn run(&self, job: &mut crate::host::Job) -> std::io::Result<crate::host::Ran> {
            LocalHost.run(job)
        }
        fn interactive(&self, argv: &[&str]) -> std::io::Result<std::process::ExitStatus> {
            LocalHost.interactive(argv)
        }
    }

    #[cfg(unix)]
    #[test]
    fn projects_are_found_the_same_through_a_host_that_only_runs_commands() {
        let dir = tree();
        for place in ["proj/.devcontainer/x", "other/.devcontainer"] {
            let path = dir.path().join(place).join("devcontainer.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").unwrap();
        }
        let s = scope(dir.path());
        let there = Files::new(&Sh, Path::new(HOME)).walk(&s, PROJECT).unwrap();
        assert_eq!(there, files().walk(&s, PROJECT).unwrap());
        assert_eq!(there.len(), 2, "proj and other");
        let kind = |h: &dyn Host| h.inspect(&dir.path().join("proj"), PROJECT.markers());
        assert_eq!(kind(&Sh).unwrap(), kind(&LocalHost).unwrap());
    }
}
