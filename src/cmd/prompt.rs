//! Asking the person at the terminal what `ldg up` can't decide on its own.

use std::io::{self, IsTerminal};

use crate::devcontainer::Prompt;
use crate::output::{line_err, note, prompt, warning};

/// Asks on the terminal; without one, takes the safe default.
pub struct Terminal;

impl Prompt for Terminal {
    fn confirm_recreate(&self, question: &str) -> bool {
        if !io::stdin().is_terminal() {
            note!(
                "{question}\n(no TTY — keeping the existing container; \
                 re-run with `ldg up --yes` or `--build` to recreate)"
            );
            return false;
        }
        prompt!("{question} [y/N] ");
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        let answer = line.trim().to_ascii_lowercase();
        answer == "y" || answer == "yes"
    }

    fn choose_workdir(&self, configured: &str, candidates: &[String]) -> String {
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
            prompt!("> ");
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
                return "/".to_string();
            }
            let input = line.trim();
            if let Ok(n) = input.parse::<usize>() {
                if (1..=candidates.len()).contains(&n) {
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
}
