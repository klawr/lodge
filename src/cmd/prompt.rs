//! Asking the person at the terminal what a command can't decide on its own.
//!
//! The terminal is a dependency like any other: a command that needs an answer
//! takes the interface for the question ([`Choose`] here, `devcontainer::Prompt`
//! for what `ldg up` asks), and only the composition root names [`Terminal`]. A
//! new kind of question is a new method on a trait and an implementation here;
//! one that needs the person (a password, say) is the same.

use anyhow::{Result, bail};
use std::io::{self, BufRead, IsTerminal};

use super::select;
use super::table::Table;
use crate::devcontainer::Prompt;
use crate::output::{line_err, note, prompt, warning};

/// Asks the person to pick one of a few things.
pub trait Choose {
    /// The index of the option picked, or `None` when the person declines. An
    /// error when there is nobody to ask.
    fn choose(&self, question: &str, options: &Table) -> Result<Option<usize>>;
}

/// Asks on the terminal; without one, takes the safe default (or, for a question
/// with no safe default, says there is nobody to ask).
pub struct Terminal;

impl Choose for Terminal {
    fn choose(&self, question: &str, options: &Table) -> Result<Option<usize>> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!("cannot ask \"{question}\" without a terminal");
        }
        match select::pick(question, options)? {
            select::Picked::Chose(i) => Ok(Some(i)),
            select::Picked::Left => Ok(None),
            // A terminal that can't give single keys (`TERM=dumb`): type a number.
            select::Picked::NoRawMode => {
                choose_from(question, &options.natural(), &mut io::stdin().lock())
            }
        }
    }
}

/// What a line typed at a numbered list means.
#[derive(Debug, PartialEq)]
enum Answer {
    Pick(usize),
    Decline,
    Again,
}

fn read_answer(line: &str, options: usize) -> Answer {
    match line.trim() {
        "" | "q" | "Q" => Answer::Decline,
        text => match text.parse::<usize>() {
            Ok(n) if (1..=options).contains(&n) => Answer::Pick(n - 1),
            _ => Answer::Again,
        },
    }
}

/// Shows `options` as a numbered list and reads the answer from `input`, asking
/// again until it is a number on the list, or empty (or `q`) to decline. The end
/// of the input declines too.
fn choose_from(
    question: &str,
    options: &[String],
    input: &mut dyn BufRead,
) -> Result<Option<usize>> {
    note!("{question}");
    let digits = options.len().to_string().len();
    for (i, option) in options.iter().enumerate() {
        line_err!("  {:>digits$}) {option}", i + 1);
    }
    loop {
        prompt!("number, or Enter to leave > ");
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match read_answer(&line, options.len()) {
            Answer::Pick(i) => return Ok(Some(i)),
            Answer::Decline => return Ok(None),
            Answer::Again => note!("Enter a number between 1 and {}.", options.len()),
        }
    }
}

impl Prompt for Terminal {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<String> {
        vec!["one".into(), "two".into(), "three".into()]
    }

    fn answer(typed: &str) -> Option<usize> {
        choose_from("which?", &options(), &mut typed.as_bytes()).unwrap()
    }

    #[test]
    fn a_number_on_the_list_picks_that_option() {
        assert_eq!(read_answer("1", 3), Answer::Pick(0));
        assert_eq!(read_answer(" 3 \n", 3), Answer::Pick(2));
        assert_eq!(answer("2\n"), Some(1));
    }

    #[test]
    fn nothing_or_q_declines() {
        assert_eq!(read_answer("", 3), Answer::Decline);
        assert_eq!(read_answer("q", 3), Answer::Decline);
        assert_eq!(answer("\n"), None);
        assert_eq!(answer(""), None, "the end of the input is no answer");
    }

    #[test]
    fn anything_else_is_asked_again() {
        for typed in ["0", "4", "-1", "two", "1 2"] {
            assert_eq!(read_answer(typed, 3), Answer::Again, "{typed}");
        }
        assert_eq!(answer("9\nfoo\n3\n"), Some(2));
        assert_eq!(answer("9\n"), None);
    }
}
