//! What could be injected, found on this machine: the known tools that are
//! installed, the other programs on `PATH`, the files a program keeps beside it
//! (see [`companions`]), and the config files and folders in the home directory.
//! Each is ready to be added as it is, so the person chooses from a list and is
//! not asked for paths.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::init::Detected;
use crate::config::edit::NewEntry;
use crate::layout::tilde;
use crate::platform;
use crate::shell::is_plain_name;

/// Something that can be added: what it is called and shown as, and the entry
/// adding it makes.
pub struct Candidate {
    pub name: String,
    pub kind: &'static str,
    pub detail: String,
    pub entry: NewEntry,
}

/// Dotfiles and folders in the home directory that are state, caches or
/// credentials, not configuration: never offered.
const NOT_OFFERED: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".claude",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".config",
    ".cache",
    ".local",
    ".cargo",
    ".rustup",
    ".npm",
    ".nvm",
    ".mozilla",
    ".var",
    ".java",
    ".m2",
    ".gradle",
    ".git",
    ".viminfo",
    ".lesshst",
    ".Xauthority",
    ".ICEauthority",
];

/// The same for `~/.config`: credentials, and what the desktop keeps there.
const NOT_OFFERED_IN_CONFIG: &[&str] = &[
    "gh",
    "gcloud",
    "op",
    "rclone",
    "glab-cli",
    "hub",
    "pulse",
    "dconf",
    "systemd",
    "google-chrome",
    "chromium",
    "BraveSoftware",
    "lodge",
];

/// What is never offered when looking through the home directory: the
/// credentials and state of [`NOT_OFFERED`], and of `~/.config`.
pub fn withheld(home: &Path) -> Vec<PathBuf> {
    let top = NOT_OFFERED.iter().filter(|n| **n != ".config");
    let config = NOT_OFFERED_IN_CONFIG.iter().map(|n| format!(".config/{n}"));
    top.map(|n| n.to_string())
        .chain(config)
        .map(|n| home.join(n))
        .collect()
}

/// The programs on offer: the known tools that are installed first, then the
/// rest of `PATH`.
pub fn programs(home: &Path, path_var: &OsStr, known: &[Detected]) -> Vec<Candidate> {
    let mut used = HashSet::new();
    let mut out = Vec::new();
    for tool in known {
        let name = tool.known.binary;
        if used.insert(name.to_string()) {
            let at = tilde(&tool.binary, home);
            out.push(Candidate {
                name: name.to_string(),
                kind: "tool",
                detail: at.clone(),
                entry: NewEntry::binary(name, at),
            });
        }
    }
    for (name, path) in path_programs(path_var) {
        if used.insert(name.clone()) {
            let at = tilde(&path, home);
            out.push(Candidate {
                detail: at.clone(),
                entry: NewEntry::binary(&name, at),
                name,
                kind: "program",
            });
        }
    }
    out
}

/// The data on offer: what the programs in the config keep beside them, then what
/// is in `~/.config` and the dotfiles in the home directory, leaving out
/// credentials and state. `binaries` are the programs of the config as `(name,
/// host path)`.
pub fn data(home: &Path, binaries: &[(String, PathBuf)]) -> Vec<Candidate> {
    let mut used = HashSet::new();
    let mut out = Vec::new();
    for (binary, path) in binaries {
        for place in companions(path) {
            if used.insert(place.clone()) {
                out.push(Candidate {
                    name: name_from(&place),
                    kind: "data",
                    detail: format!("{} · for {binary}", tilde(&place, home)),
                    entry: NewEntry::data(tilde(&place, home), None, Some(binary.clone())),
                });
            }
        }
    }
    let mut paths = children(&home.join(".config"), NOT_OFFERED_IN_CONFIG);
    paths.extend(children(home, NOT_OFFERED).into_iter().filter(|p| {
        p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with('.') && !n.ends_with("history")
        })
    }));
    for path in paths {
        let name = name_from(&path);
        if name.is_empty() || !used.insert(path.clone()) {
            continue;
        }
        out.push(Candidate {
            kind: "config",
            detail: tilde(&path, home),
            entry: NewEntry::data(tilde(&path, home), None, None),
            name,
        });
    }
    out
}

/// A name for an entry made from a path: its last part without leading dots
/// (`~/.bashrc` is `bashrc`), cut to what a name may hold.
pub(super) fn name_from(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().trim_start_matches('.').to_string())
        .unwrap_or_default();
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.+".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// What a program keeps beside it, by the filesystem layout's convention and
/// without knowing anything about the program: for `<prefix>/bin/<name>`, the
/// directories `<prefix>/share/<name>` and `<prefix>/lib/<name>` that exist. A
/// program built to run from where it was installed (an editor with its runtime
/// files, a tool with its plugins) looks there, so it is not much use without them.
pub fn companions(binary: &Path) -> Vec<PathBuf> {
    let (Some(name), Some(bin)) = (binary.file_name(), binary.parent()) else {
        return Vec::new();
    };
    let Some(prefix) = bin
        .parent()
        .filter(|_| bin.file_name().is_some_and(|n| n == "bin"))
    else {
        return Vec::new();
    };
    ["share", "lib"]
        .iter()
        .map(|dir| prefix.join(dir).join(name))
        .filter(|place| place.is_dir())
        .collect()
}

/// The entries of `dir`, sorted, except `skip`; nothing when it can't be read.
fn children(dir: &Path, skip: &[&str]) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| !skip.contains(&n.to_string_lossy().as_ref()))
        })
        .collect();
    paths.sort();
    paths
}

/// The executables on `PATH`, by name, the first of each name winning as it does
/// when the shell runs it.
fn path_programs(path_var: &OsStr) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for dir in std::env::split_paths(path_var) {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<(String, PathBuf)> = read
            .filter_map(Result::ok)
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
            .collect();
        names.sort();
        for (name, path) in names {
            if platform::is_executable(&path) && is_plain_name(&name) && seen.insert(name.clone()) {
                found.push((name, path));
            }
        }
    }
    found.sort();
    found
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn touch(path: &Path, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
        crate::platform::set_mode(path, mode).unwrap();
    }

    fn names(c: &[Candidate]) -> Vec<&str> {
        c.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn configs_are_offered_and_credentials_and_state_are_not() {
        let dir = TempDir::new("candidates").unwrap();
        let home = dir.path().join("home");
        for f in [
            ".bashrc",
            ".tmux.conf",
            ".bash_history",
            ".config/nvim/init.lua",
        ] {
            touch(&home.join(f), 0o644);
        }
        touch(&home.join(".ssh/id_ed25519"), 0o600);
        touch(&home.join(".config/gh/hosts.yml"), 0o600);
        touch(&home.join(".config/starship.toml"), 0o644);
        let found = data(&home, &[]);
        let all = names(&found);
        for want in ["nvim", "starship.toml", "bashrc", "tmux.conf"] {
            assert!(all.contains(&want), "{want} missing from {all:?}");
        }
        for never in ["gh", "ssh", "id_ed25519", "bash_history"] {
            assert!(!all.contains(&never), "{never} in {all:?}");
        }
        // Data goes where it is here, so it is added with no target of its own.
        let rc = found.iter().find(|c| c.name == "bashrc").unwrap();
        let NewEntry::Data(rc) = &rc.entry else {
            panic!("a dotfile is data");
        };
        assert_eq!(rc.path, "~/.bashrc");
        assert_eq!(rc.target, None);
    }

    #[test]
    fn a_program_keeps_its_files_in_share_and_lib_beside_its_bin() {
        let dir = TempDir::new("candidates-companions").unwrap();
        let prefix = dir.path().join("local");
        touch(&prefix.join("bin/nvim"), 0o755);
        touch(&prefix.join("share/nvim/runtime/init.vim"), 0o644);
        touch(&prefix.join("lib/nvim/parser/c.so"), 0o644);
        touch(&prefix.join("share/other/x"), 0o644);
        touch(&prefix.join("lib/nvim-not-a-dir"), 0o644);
        assert_eq!(
            companions(&prefix.join("bin/nvim")),
            [prefix.join("share/nvim"), prefix.join("lib/nvim")]
        );
        // A program with nothing beside it, and one that is not in a `bin/`.
        touch(&prefix.join("bin/rg"), 0o755);
        touch(&dir.path().join("tools/nvim"), 0o755);
        touch(&dir.path().join("share/nvim/x"), 0o644);
        assert!(companions(&prefix.join("bin/rg")).is_empty());
        assert!(companions(&dir.path().join("tools/nvim")).is_empty());
    }

    #[test]
    fn the_data_of_the_programs_in_the_config_comes_first_and_says_what_it_is_for() {
        let dir = TempDir::new("candidates-data").unwrap();
        let home = dir.path().join("home");
        touch(&home.join(".bashrc"), 0o644);
        touch(&home.join("opt/bin/tool"), 0o755);
        touch(&home.join("opt/share/tool/x"), 0o644);
        let programs = [("tool".to_string(), home.join("opt/bin/tool"))];
        let found = data(&home, &programs);
        let NewEntry::Data(first) = &found[0].entry else {
            panic!("what sits beside a program is data");
        };
        assert_eq!(first.path, "~/opt/share/tool");
        assert_eq!(first.binary.as_deref(), Some("tool"));
        assert!(found[0].detail.ends_with("for tool"), "{}", found[0].detail);
        assert!(names(&found).contains(&"bashrc"));
    }

    #[test]
    fn programs_are_the_executables_on_path_once_each_and_known_tools_come_first() {
        let dir = TempDir::new("candidates-programs").unwrap();
        touch(&dir.path().join("bin/rg"), 0o755);
        touch(&dir.path().join("bin/notes.txt"), 0o644);
        touch(&dir.path().join("other/rg"), 0o755);
        touch(&dir.path().join("other/fd"), 0o755);
        let path =
            std::env::join_paths([dir.path().join("bin"), dir.path().join("other")]).unwrap();
        let found = programs(dir.path(), &path, &[]);
        assert_eq!(names(&found), vec!["fd", "rg"]);
        assert!(found[1].detail.ends_with("bin/rg"), "the first folder wins");
        assert!(
            found
                .iter()
                .all(|c| c.entry.kind() == crate::config::edit::Kind::Binary)
        );
    }
}
