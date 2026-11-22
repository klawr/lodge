//! What could be injected, found on this machine: the known tools that are
//! installed (with their configs), the other programs on `PATH`, and the config
//! files and folders in the home directory. Each is ready to be added as it
//! is, so the person chooses from a list and is not asked for paths.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::init::{Detected, tilde};
use crate::config::edit::NewEntry;
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

/// The configs on offer: what is in `~/.config` and the dotfiles in the home
/// directory, leaving out credentials and state.
pub fn configs(home: &Path) -> Vec<Candidate> {
    let mut used = HashSet::new();
    let mut paths = children(&home.join(".config"), NOT_OFFERED_IN_CONFIG);
    paths.extend(children(home, NOT_OFFERED).into_iter().filter(|p| {
        p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with('.') && !n.ends_with("history")
        })
    }));
    let mut out = Vec::new();
    for path in paths {
        let name = super::inject::name_from(&path);
        if name.is_empty() || !used.insert(name.clone()) {
            continue;
        }
        out.push(Candidate {
            kind: "config",
            detail: tilde(&path, home),
            entry: config_entry(&name, &path, home),
            name,
        });
    }
    out
}

/// A config file or folder, which goes where it is here when that is under `home`.
fn config_entry(name: &str, path: &Path, home: &Path) -> NewEntry {
    NewEntry::config(name, tilde(path, home), target_in_home(path, home))
}

/// A config's place in the container: the same as here, when it is under the home
/// directory (the container's user has one too); else it is left to the default.
fn target_in_home(path: &Path, home: &Path) -> Option<String> {
    let at = tilde(path, home);
    at.starts_with("~/").then_some(at)
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
            let runnable = std::fs::metadata(&path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if runnable && is_plain_name(&name) && seen.insert(name.clone()) {
                found.push((name, path));
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmpfile::TempDir;

    fn touch(path: &Path, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
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
        let found = configs(&home);
        let all = names(&found);
        for want in ["nvim", "starship.toml", "bashrc", "tmux.conf"] {
            assert!(all.contains(&want), "{want} missing from {all:?}");
        }
        for never in ["gh", "ssh", "id_ed25519", "bash_history"] {
            assert!(!all.contains(&never), "{never} in {all:?}");
        }
        // A config goes where it is here, when that is under the home directory.
        let rc = found.iter().find(|c| c.name == "bashrc").unwrap();
        assert_eq!(rc.entry.path, "~/.bashrc");
        assert_eq!(rc.entry.target.as_deref(), Some("~/.bashrc"));
        assert_eq!(rc.entry.kind, crate::config::edit::Kind::Config);
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
                .all(|c| c.entry.kind == crate::config::edit::Kind::Binary)
        );
    }
}
