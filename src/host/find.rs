//! Looking for files and folders on a host, for a person who is choosing one.
//!
//! A [`Search`] says what is wanted and where, and [`Host::find`](super::Host::find)
//! answers it. The policy is one: what is skipped, how deep a search goes, how
//! many entries it keeps, what counts as a project and in which order the answers
//! come. Only the way of listing differs. This machine walks its own folders
//! ([`walk`]); any other host runs one `find` on its side ([`script`]) and has
//! the answer shaped by the same rules ([`shape`]), so a project found on
//! another machine is found the way a local one would be.
//!
//! Paths of another machine are kept as the text it printed: they are never
//! joined or made absolute here, because a client on Windows would turn them into
//! Windows paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::local::expand_local;
use crate::shell::shell_quote;

/// The files that make a folder what it is, each as its path components from the
/// folder; a `*` is any one name. A folder with any of them is marked. What they
/// are is the caller's to say (for a project, where its devcontainer config is
/// kept): nothing here knows.
pub type Markers = &'static [&'static [&'static str]];

/// What counts as an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    File,
    /// A file or a folder.
    Any,
    /// A folder with one of these files: a project.
    Project(Markers),
}

impl Want {
    /// What marks a folder as the answer; nothing, when no folder is.
    pub fn markers(self) -> Markers {
        match self {
            Want::Project(markers) => markers,
            Want::File | Want::Any => &[],
        }
    }
}

/// How far down a search goes, and how many entries it keeps: enough for a home
/// directory, little enough to be drawn at once.
pub const DEPTH: usize = 5;
pub const LIMIT: usize = 20_000;

/// What is looked for: a whitelist of roots, then a blacklist of names (at any
/// depth) and paths. The blacklist is only for searching: a path that is named
/// in full is never refused by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub roots: Vec<PathBuf>,
    pub denied_names: Vec<String>,
    pub denied_paths: Vec<PathBuf>,
    pub want: Want,
}

impl Search {
    fn denies(&self, path: &Path, name: &str) -> bool {
        self.denied_names.iter().any(|n| n == name) || self.denied_paths.iter().any(|d| d == path)
    }

    /// Projects are not in hidden folders.
    fn hides(&self, name: &str) -> bool {
        matches!(self.want, Want::Project(_)) && name.starts_with('.')
    }
}

/// One answer: a path, and whether it is a folder (a link to one is not: links
/// are listed and never followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub dir: bool,
}

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Missing,
    File,
    Folder,
    /// A folder with one of the [`Markers`] asked about.
    Project,
}

impl Kind {
    pub fn is_folder(self) -> bool {
        matches!(self, Kind::Folder | Kind::Project)
    }
}

/// `typed` with a leading `~` replaced by `home`, as text. Where a relative path
/// begins is the host's business ([`Host::resolve`](super::Host::resolve)).
pub fn expand_home(typed: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    match typed {
        "~" => home.to_string(),
        _ => match typed.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => typed.to_string(),
        },
    }
}

// ── this machine ────────────────────────────────────────────────────────────

/// What is wanted under the roots of `search`, shallowest first and otherwise in
/// name order. Breadth first, leaves out what the search denies, and stops at a
/// depth and a count. Links are listed but not followed, so a walk cannot go
/// round in a circle. A root that cannot be read has nothing in it.
pub fn walk(search: &Search) -> Vec<Entry> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for root in &search.roots {
        let mut level = vec![root.clone()];
        for _ in 0..DEPTH {
            let mut next = Vec::new();
            for dir in &level {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if search.denies(&path, &name) {
                        continue;
                    }
                    let dir = entry.file_type().is_ok_and(|t| t.is_dir());
                    let hidden = search.hides(&name);
                    if dir && !hidden {
                        next.push(path.clone());
                    }
                    let wanted = match search.want {
                        Want::File => !dir,
                        Want::Any => true,
                        Want::Project(markers) => dir && !hidden && marked(&path, markers),
                    };
                    if wanted && seen.insert(path.clone()) {
                        found.push(Entry { path, dir });
                        if found.len() >= LIMIT {
                            return found;
                        }
                    }
                }
            }
            level = next;
        }
    }
    found
}

/// Whether `dir`, on this machine, holds one of the `markers`.
pub fn marked(dir: &Path, markers: Markers) -> bool {
    markers
        .iter()
        .any(|marker| !expand_local(&dir.join(marker.iter().collect::<PathBuf>())).is_empty())
}

/// What `path` is on this machine.
pub fn inspect(path: &Path, markers: Markers) -> Kind {
    if path.is_dir() {
        if marked(path, markers) {
            Kind::Project
        } else {
            Kind::Folder
        }
    } else if path.exists() {
        Kind::File
    } else {
        Kind::Missing
    }
}

// ── another machine ─────────────────────────────────────────────────────────

/// How much of a listing is read: a search that would print more is cut off, the
/// way [`LIMIT`] cuts a local one.
const MAX_BYTES: usize = 8 << 20;

/// The `sh` script behind [`Host::find`](super::Host::find): one `find` that
/// prints each entry as a type letter and the path, ended by a NUL (`d` a folder,
/// `f` anything else), the blacklist pruned on the host's side so that it is
/// never walked. For a project it prints the config files instead, from which
/// [`shape`] names the projects: hidden folders are skipped, except the one a
/// config is kept in.
pub fn script(search: &Search) -> String {
    let roots: Vec<String> = search.roots.iter().map(|r| quote(r)).collect();
    let mut prune: Vec<String> = search
        .denied_names
        .iter()
        .map(|n| format!("-name {}", shell_quote(&glob_escape(n))))
        .collect();
    prune.extend(
        search
            .denied_paths
            .iter()
            .map(|p| format!("-path {}", shell_quote(&glob_escape(&p.to_string_lossy())))),
    );
    let denied = if prune.is_empty() {
        String::new()
    } else {
        format!("\\( {} \\) -prune -o ", prune.join(" -o "))
    };
    let (depth, rest) = match search.want {
        Want::Project(markers) => (
            // A marker is as many folders below its project as it has parts, less
            // the file itself.
            DEPTH + markers.iter().map(|m| m.len()).max().unwrap_or(0),
            marker_files(markers),
        ),
        Want::File | Want::Any => (
            DEPTH,
            "\\( -type d -exec printf 'd%s\\0' {} + \\) -o -exec printf 'f%s\\0' {} +".to_string(),
        ),
    };
    format!(
        "find -H {} -mindepth 1 -maxdepth {depth} {denied}{rest} | head -c {MAX_BYTES}",
        roots.join(" ")
    )
}

fn quote(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

/// `text` as a `find` pattern that matches exactly itself.
fn glob_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '?' | '[') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The `find` expression that prints the files of `markers` (by their names, the
/// folders between decided later by [`project_of`]), skipping hidden folders but
/// those a marker passes through.
fn marker_files(markers: Markers) -> String {
    let mut names: Vec<&str> = markers.iter().filter_map(|m| m.last().copied()).collect();
    names.dedup();
    let named: Vec<String> = names
        .iter()
        .map(|n| format!("-name {}", shell_quote(&glob_escape(n))))
        .collect();
    let mut passed: Vec<&str> = markers
        .iter()
        .flat_map(|m| m.iter().take(m.len().saturating_sub(1)))
        .copied()
        .filter(|c| *c != "*")
        .collect();
    passed.dedup();
    let except: String = passed
        .iter()
        .map(|c| format!(" ! -name {}", shell_quote(&glob_escape(c))))
        .collect();
    format!(
        "\\( -type d -name '.*'{except} \\) -prune -o \\\
         -type f \\( {} \\) -exec printf 'f%s\\0' {{}} +",
        named.join(" -o ")
    )
}

/// The answers of `search` from what [`script`] printed: the same entries, in
/// the same order, that [`walk`] gives for the same folders. An entry cut off by
/// the end of the output is dropped.
pub fn shape(search: &Search, printed: &[u8]) -> Vec<Entry> {
    let mut raw: Vec<Entry> = Vec::new();
    let mut parts: Vec<&[u8]> = printed.split(|&b| b == 0).collect();
    // What follows the last NUL is nothing, or was cut off.
    parts.pop();
    for part in parts {
        let Some((&kind, path)) = part.split_first() else {
            continue;
        };
        let path = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        raw.push(Entry {
            path,
            dir: kind == b'd',
        });
    }
    let mut found: Vec<(usize, usize, Entry)> = Vec::new();
    let mut seen = HashSet::new();
    for entry in raw {
        // A config file stands for its project.
        let entry = match search.want {
            Want::Project(markers) => {
                project_of(&entry.path, markers).map(|path| Entry { path, dir: true })
            }
            Want::File | Want::Any => Some(entry),
        };
        let Some(entry) = entry else { continue };
        let Some((root, depth)) = place(search, &entry.path) else {
            continue;
        };
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let wanted = match search.want {
            Want::File => !entry.dir,
            Want::Any => true,
            Want::Project(_) => (1..=DEPTH).contains(&depth) && !search.hides(&name),
        };
        if wanted && !search.denies(&entry.path, &name) && seen.insert(entry.path.clone()) {
            found.push((root, depth, entry));
        }
    }
    // The order of a breadth-first walk: root by root, nearest first, then by name.
    found.sort_by(|a, b| (a.0, a.1, &a.2.path).cmp(&(b.0, b.1, &b.2.path)));
    found.truncate(LIMIT);
    found.into_iter().map(|(_, _, e)| e).collect()
}

/// The root `path` is under, by its place in the list, and how many names deep.
fn place(search: &Search, path: &Path) -> Option<(usize, usize)> {
    search.roots.iter().enumerate().find_map(|(i, root)| {
        let rest = path.strip_prefix(root).ok()?;
        Some((i, rest.components().count()))
    })
}

/// The folder `file` marks, when it is one of the `markers` of it.
fn project_of(file: &Path, markers: Markers) -> Option<PathBuf> {
    markers.iter().find_map(|marker| {
        let mut dir = file;
        for part in marker.iter().rev() {
            let name = dir.file_name()?.to_str()?;
            if *part != "*" && *part != name {
                return None;
            }
            dir = dir.parent()?;
        }
        Some(dir.to_path_buf())
    })
}

/// The `sh` script behind [`Host::inspect`](super::Host::inspect): prints
/// `project`, `folder`, `file` or `missing`.
pub fn inspect_script(path: &Path, markers: Markers) -> String {
    // Each marker as a word for the shell to expand: its parts quoted, a `*` bare.
    let words: Vec<String> = markers
        .iter()
        .map(|m| {
            let parts: Vec<String> = m
                .iter()
                .map(|c| {
                    if *c == "*" {
                        "*".into()
                    } else {
                        shell_quote(c)
                    }
                })
                .collect();
            format!("\"$p\"/{}", parts.join("/"))
        })
        .collect();
    let marked = if words.is_empty() {
        String::new()
    } else {
        format!(
            "for f in {}; do [ -f \"$f\" ] && k=project && break; done; ",
            words.join(" ")
        )
    };
    format!(
        "p={}; if [ -d \"$p\" ]; then k=folder; {marked}echo $k; \
         elif [ -e \"$p\" ]; then echo file; else echo missing; fi",
        quote(path)
    )
}

/// The [`Kind`] [`inspect_script`] printed.
pub fn parse_kind(printed: &str) -> Option<Kind> {
    match printed.trim() {
        "project" => Some(Kind::Project),
        "folder" => Some(Kind::Folder),
        "file" => Some(Kind::File),
        "missing" => Some(Kind::Missing),
        _ => None,
    }
}

/// What the tests mark a folder by: the same three shapes a project's config has
/// (a file in a folder, a file beside, a file one folder further in), under names
/// of their own, because nothing in this layer knows what a project is.
#[cfg(test)]
const MARKERS: Markers = &[
    &[".proj", "marker"],
    &[".marker"],
    &[".proj", "*", "marker"],
];
#[cfg(test)]
const PROJECT: Want = Want::Project(MARKERS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_cut_off_in_the_middle_of_an_entry_drops_that_entry() {
        let s = Search {
            roots: vec!["/r".into()],
            denied_names: vec![],
            denied_paths: vec![],
            want: Want::File,
        };
        let found = shape(&s, b"f/r/a\0f/r/b\0f/r/c");
        let paths: Vec<_> = found.iter().map(|e| e.path.display().to_string()).collect();
        assert_eq!(paths, ["/r/a", "/r/b"]);
    }

    #[test]
    fn paths_of_another_machine_stay_as_it_printed_them() {
        let s = Search {
            roots: vec!["/home/me".into()],
            denied_names: vec![],
            denied_paths: vec![],
            want: PROJECT,
        };
        let found = shape(&s, b"f/home/me/p/.proj/marker\0");
        assert_eq!(found[0].path.to_string_lossy(), "/home/me/p");
        assert!(found[0].dir);
    }

    #[test]
    fn a_tilde_is_the_home_and_nothing_else_is() {
        let home = Path::new("/home/me/");
        assert_eq!(expand_home("~", home), "/home/me");
        assert_eq!(expand_home("~/work", home), "/home/me/work");
        assert_eq!(expand_home("~other", home), "~other");
        assert_eq!(expand_home("/abs", home), "/abs");
        assert_eq!(expand_home("rel/x", home), "rel/x");
    }
}

/// Against a real `sh` and `find`, which a Unix machine has.
#[cfg(all(test, unix))]
mod against_sh {
    use super::*;
    use crate::host::{Host, Job, LocalHost, Ran};
    use crate::tmpfile::TempDir;
    use std::process::ExitStatus;

    /// A host that only runs commands, locally: its `find` is the script, not the
    /// local walk.
    struct ViaSh;

    impl Host for ViaSh {
        fn label(&self) -> Option<String> {
            Some("via-sh".into())
        }
        fn names(&self) -> Vec<String> {
            vec!["via-sh".into()]
        }
        fn shares_cwd(&self) -> bool {
            false
        }
        fn run(&self, job: &mut Job) -> std::io::Result<Ran> {
            LocalHost.run(job)
        }
        fn interactive(&self, argv: &[&str]) -> std::io::Result<ExitStatus> {
            LocalHost.interactive(argv)
        }
    }

    fn tree() -> TempDir {
        let dir = TempDir::new("find").unwrap();
        for file in [
            "top.txt",
            "a/one.txt",
            "a/b/two.txt",
            "node_modules/x/y.js",
            "build/out.o",
            ".git/HEAD",
            ".ssh/id_rsa",
            "with space/it's.txt",
            "proj/.proj/marker",
            "proj/src/main.rs",
            "dot/.marker",
            "multi/.proj/x/marker",
            "deep/er/proj2/.proj/marker",
            "empty/.proj/readme",
            ".hidden/.marker",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").unwrap();
        }
        dir
    }

    fn search(root: &Path, want: Want) -> Search {
        Search {
            roots: vec![root.to_path_buf()],
            denied_names: ["node_modules", "build", ".git"].map(String::from).to_vec(),
            denied_paths: vec![root.join(".ssh")],
            want,
        }
    }

    #[test]
    fn a_search_on_another_machine_finds_what_a_walk_here_finds() {
        let dir = tree();
        for want in [Want::File, Want::Any, PROJECT] {
            let s = search(dir.path(), want);
            let walked = LocalHost.find(&s).unwrap();
            let asked = ViaSh.find(&s).unwrap();
            assert_eq!(walked, asked, "{want:?}");
            assert!(!walked.is_empty());
        }
    }

    #[test]
    fn several_roots_come_root_by_root_without_a_path_twice() {
        let dir = tree();
        let mut s = search(dir.path(), Want::File);
        s.roots = vec![dir.path().join("a"), dir.path().join("a/b")];
        let walked = LocalHost.find(&s).unwrap();
        assert_eq!(walked, ViaSh.find(&s).unwrap());
        let two = dir.path().join("a/b/two.txt");
        assert_eq!(walked.iter().filter(|e| e.path == two).count(), 1);
    }

    #[test]
    fn projects_are_found_by_where_their_config_is_and_not_in_hidden_folders() {
        let dir = tree();
        let names: Vec<String> = ViaSh
            .find(&search(dir.path(), PROJECT))
            .unwrap()
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(dir.path())
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        assert_eq!(names, ["dot", "multi", "proj", "deep/er/proj2"]);
    }

    #[test]
    fn a_root_that_is_a_link_is_searched_and_a_missing_one_has_nothing() {
        let dir = tree();
        let link = dir.path().join("link");
        crate::platform::symlink(dir.path().join("a"), &link).unwrap();
        let mut s = search(dir.path(), Want::File);
        s.roots = vec![link, dir.path().join("nope")];
        let asked = ViaSh.find(&s).unwrap();
        assert_eq!(asked, LocalHost.find(&s).unwrap());
        assert_eq!(asked.len(), 2);
    }

    #[test]
    fn names_with_glob_characters_are_denied_literally() {
        let dir = tree();
        std::fs::create_dir_all(dir.path().join("[x]/in")).unwrap();
        std::fs::write(dir.path().join("[x]/in/f"), "").unwrap();
        std::fs::write(dir.path().join("x"), "").unwrap();
        let mut s = search(dir.path(), Want::File);
        s.denied_names = vec!["[x]".into()];
        let asked = ViaSh.find(&s).unwrap();
        assert_eq!(asked, LocalHost.find(&s).unwrap());
        assert!(asked.iter().any(|e| e.path == dir.path().join("x")));
        assert!(
            !asked
                .iter()
                .any(|e| e.path.starts_with(dir.path().join("[x]")))
        );
    }

    #[test]
    fn what_a_path_is_is_the_same_here_and_there() {
        let dir = tree();
        for (path, kind) in [
            ("proj", Kind::Project),
            ("dot", Kind::Project),
            ("multi", Kind::Project),
            ("a", Kind::Folder),
            ("empty", Kind::Folder),
            ("top.txt", Kind::File),
            ("nope", Kind::Missing),
            ("with space", Kind::Folder),
        ] {
            let path = dir.path().join(path);
            assert_eq!(LocalHost.inspect(&path, MARKERS).unwrap(), kind, "{path:?}");
            assert_eq!(ViaSh.inspect(&path, MARKERS).unwrap(), kind, "{path:?}");
        }
    }
}
