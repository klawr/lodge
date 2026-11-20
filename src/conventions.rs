//! Architecture rules, checked by the test suite so they can't quietly erode.
//!
//! Each external program lodge runs on the user's behalf is started from one
//! place. That keeps quoting, tracing, connection reuse and the local/remote
//! decision in one spot instead of copied around.

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

/// The non-test source of every file that spawns `program`.
fn spawners(program: &str) -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let needle = format!("Command::new(\"{program}\")");
    let mut found: Vec<String> = files
        .iter()
        .filter(|f| !f.ends_with("conventions.rs"))
        .filter(|f| {
            let text = std::fs::read_to_string(f).unwrap();
            // Only code before the test module counts.
            let code = text.split("#[cfg(test)]").next().unwrap();
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
fn local_shells_are_started_by_target_and_the_session_launcher_only() {
    // `target.rs` runs scripts on the project's host; `container.rs` launches the
    // user's interactive session, which is a local command line by nature.
    assert_eq!(spawners("sh"), ["container.rs", "target.rs"]);
}
