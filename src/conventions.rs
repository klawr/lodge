//! Architecture rules, checked by the test suite so they can't quietly erode.
//!
//! Each external program lodge runs on the user's behalf is started from one
//! place. That keeps quoting, tracing, connection reuse and the local/remote
//! decision in one spot instead of copied around. And dependencies point one
//! way: the command layer on top, lodge's features below it, and a foundation
//! (running things, quoting, paths, output) that knows none of them.

#![cfg(test)]

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The non-test source of every file that spawns `program`, found by the literal
/// text `Command::new("<program>")`. A program name built at run time (a
/// variable, a `host_tools` entry) is not seen here; that is what the registry
/// and the single private `ssh::base` are for.
fn spawners(program: &str) -> Vec<String> {
    // Read at run time: `env!` would bake in the path at build time, and cargo
    // doesn't rebuild when the checkout moves, so a renamed folder broke this.
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let src = Path::new(&root).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let needle = format!("Command::new(\"{program}\")");
    let mut found: Vec<String> = files
        .iter()
        .filter(|f| !f.ends_with("conventions.rs"))
        .filter(|f| {
            let text = std::fs::read_to_string(f).unwrap();
            // Only code before the test module counts. (Not before the first
            // `#[cfg(test)]`: that may be a test-only item above real code.)
            let code = text.split("#[cfg(test)]\nmod ").next().unwrap();
            code.contains(&needle)
        })
        .map(|f| f.strip_prefix(&src).unwrap().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

#[test]
fn only_ssh_rs_starts_ssh() {
    assert_eq!(spawners("ssh"), ["ssh.rs"]);
}

#[test]
fn only_the_docker_runtime_starts_docker() {
    assert_eq!(spawners("docker"), ["runtime/docker/mod.rs"]);
}

#[test]
fn local_shells_are_started_by_target_secrets_and_the_session_launcher_only() {
    // `target.rs` runs scripts on the project's host; `secret/` runs a secret's
    // `command` source, which is on this machine whatever the target;
    // `session/` launches the user's interactive session, which is a local
    // command line by nature.
    assert_eq!(
        spawners("sh"),
        ["secret/mod.rs", "session/mod.rs", "target.rs"]
    );
}

#[test]
fn host_tools_are_started_through_the_registry_only() {
    // `host_tools::Tool::command` is the one place; nobody spells them out.
    for tool in crate::host_tools::ALL {
        if tool.name == "ssh" {
            continue; // ssh has its own module, checked above.
        }
        assert_eq!(
            spawners(tool.name),
            Vec::<String>::new(),
            "start {} through host_tools",
            tool.name
        );
    }
    // lodge no longer needs a local `tar`: features and remote copies unpack
    // in-process.
    assert_eq!(spawners("tar"), Vec::<String>::new());
}

/// The top-level modules each non-test source file names through `crate::`,
/// keyed by the module the file belongs to. Comments are left out, so a doc
/// link is not a dependency.
fn dependencies() -> Vec<(String, String, String)> {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let src = Path::new(&root).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut out = Vec::new();
    for f in files {
        let rel = f.strip_prefix(&src).unwrap().to_string_lossy().into_owned();
        let module = rel
            .split('/')
            .next()
            .unwrap()
            .trim_end_matches(".rs")
            .to_string();
        let text = std::fs::read_to_string(&f).unwrap();
        let code = text.split("#[cfg(test)]\nmod ").next().unwrap();
        for line in code.lines().filter(|l| !l.trim_start().starts_with("//")) {
            for (i, _) in line.match_indices("crate::") {
                let rest = &line[i + "crate::".len()..];
                // `crate::a::…`, or a group: `crate::{a, b::c}`.
                let names: Vec<&str> = match rest.strip_prefix('{') {
                    Some(group) => group.split('}').next().unwrap().split(',').collect(),
                    None => vec![rest],
                };
                for name in names {
                    let named: String = name
                        .trim()
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !named.is_empty() && named != module {
                        out.push((rel.clone(), module.clone(), named));
                    }
                }
            }
        }
    }
    out
}

/// What everything else is built on: running things, quoting, paths, output.
/// None of it knows about lodge's features.
const FOUNDATION: &[&str] = &[
    "fingerprint",
    "host_tools",
    "layout",
    "output",
    "port",
    "runtime",
    "shell",
    "ssh",
    "target",
    "tmpfile",
    "tree",
];

#[test]
fn the_foundation_depends_on_nothing_above_it() {
    let wrong: Vec<String> = dependencies()
        .into_iter()
        .filter(|(_, from, to)| {
            FOUNDATION.contains(&from.as_str()) && !FOUNDATION.contains(&to.as_str())
        })
        .map(|(file, _, to)| format!("{file} uses crate::{to}"))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn only_the_command_layer_knows_the_command_line() {
    // `main.rs` hands the parsed `cli` to `cmd`; `doc_facts.rs` checks the docs
    // against it.
    let wrong: Vec<String> = dependencies()
        .into_iter()
        .filter(|(file, from, to)| {
            matches!(to.as_str(), "cmd" | "cli")
                && from != "cmd"
                && !matches!(file.as_str(), "main.rs" | "doc_facts.rs")
        })
        .map(|(file, _, to)| format!("{file} uses crate::{to}"))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}
