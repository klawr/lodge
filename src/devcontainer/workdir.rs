//! Picking the directory to land in when the configured `workspaceFolder`
//! doesn't exist inside the container.

use crate::output::{line_err, note, prompt, warning};
use std::io::{self, IsTerminal};

use crate::runtime::Container;

// Called when the configured workspaceFolder doesn't exist in the container.
// Shows mount destinations and /workspaces/* entries as numbered candidates and
// lets the user pick one or type a custom path. Falls back to '/' without
// prompting when stdin is not a terminal.
pub(super) fn select_workdir(container: &dyn Container, configured: &str) -> String {
    let candidates = discover_workdirs(container);

    if candidates.is_empty() || !io::stdin().is_terminal() {
        warning!(
            "workspaceFolder '{configured}' not found in the container; falling back to '/'.\n\
               Add \"workspaceFolder\": \"<path>\" to devcontainer.json to avoid this."
        );
        return "/".to_string();
    }

    note!("workspaceFolder '{configured}' not found in the container.");
    note!("Select a workspace directory (or press Enter for '/'):");
    for (i, dir) in candidates.iter().enumerate() {
        line_err!("  {}) {dir}", i + 1);
    }

    loop {
        prompt!("[lodge] > ");
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
            return "/".to_string();
        }
        let input = line.trim();
        if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= candidates.len() {
                return candidates[n - 1].clone();
            }
            note!(
                "Enter a number between 1 and {} or an absolute path.",
                candidates.len()
            );
        } else if input.starts_with('/') {
            return input.to_string();
        } else {
            note!("Enter a number from the list or an absolute path starting with '/'.");
        }
    }
}

// Collects workspace candidates: actual mount destinations from `docker inspect`
// (the most relevant — the user's workspace is almost always a bind mount), plus
// any subdirectories found under /workspaces and /workspace (the standard
// devcontainer locations). Sorted and deduplicated.
fn discover_workdirs(container: &dyn Container) -> Vec<String> {
    let mut candidates = container.mounts();

    if let Ok(out) = container.exec_capture(&[
        "sh",
        "-c",
        "find /workspaces /workspace -maxdepth 1 -mindepth 1 -type d 2>/dev/null",
    ]) {
        for line in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
            let s = line.to_string();
            if !candidates.contains(&s) {
                candidates.push(s);
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}
