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

pub use super::select::{Doc, DocLine, Hotkey, Item, Knob, Pick};

/// How a question with hotkeys was answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Row(usize),
    /// A hotkey, with the row that was selected when it was pressed.
    Key(char, usize),
    Left,
}

/// What came of looking through a list of things.
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// The indexes of the items chosen.
    Items(Vec<usize>),
    /// A path typed in full.
    Path(String),
    /// A knob was edited, so the list is to be made again with the change.
    Edited(usize, String),
    Left,
    /// The terminal can't do this; ask for what is wanted in words instead.
    Unsupported,
}

/// Asks the person to pick one of a few things.
pub trait Choose {
    /// The index of the option picked, or `None` when the person declines. An
    /// error when there is nobody to ask.
    fn choose(&self, question: &str, options: &Table) -> Result<Option<usize>>;

    /// The same, with keys of their own: pressing one answers at once. Without a
    /// terminal that gives single keys there are no hotkeys, and the answer is
    /// a row or nothing.
    fn choose_with(&self, question: &str, options: &Table, _hotkeys: &[Hotkey]) -> Result<Choice> {
        Ok(match self.choose(question, options)? {
            Some(i) => Choice::Row(i),
            None => Choice::Left,
        })
    }

    /// Shows `doc` and lets the person move over its blocks, with hotkeys as in
    /// [`Choose::choose_with`] (the row of a key, and of Enter, is the line selected, counted over those that can be). `None`
    /// when the terminal can't show it: the caller asks some other way.
    fn view(&self, _title: &str, _doc: &Doc, _hotkeys: &[Hotkey]) -> Result<Option<Choice>> {
        Ok(None)
    }

    /// Looks through `items` like [`Choose::find`], but each thing chosen is added at
    /// once through `add` (which says what it did, or why it could not), marked in
    /// the list, and the search stays open for the next. `false` when the terminal
    /// can't do this: the caller asks some other way.
    fn find_adding(
        &self,
        _question: &str,
        _tabs: &[&str],
        _items: &[Item],
        _add: &mut dyn FnMut(Pick) -> Result<String, String>,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Looks through `items`, in lists named `tabs` that the person switches
    /// between, narrowing them as they type, and returns the ones marked.
    fn find(&self, _question: &str, _tabs: &[&str], _items: &[Item]) -> Result<Selection> {
        Ok(Selection::Unsupported)
    }

    /// [`Choose::find`], with settings of the search that the person can edit
    /// from inside it. Without a terminal that can, the settings are not offered.
    fn find_knobs(
        &self,
        question: &str,
        tabs: &[&str],
        items: &[Item],
        _knobs: &[Knob],
    ) -> Result<Selection> {
        self.find(question, tabs, items)
    }

    /// [`Choose::find_adding`], with settings of the search as in [`Choose::find_knobs`].
    fn find_adding_knobs(
        &self,
        question: &str,
        tabs: &[&str],
        items: &[Item],
        _knobs: &[Knob],
        add: &mut dyn FnMut(Pick) -> Result<String, String>,
    ) -> Result<Adding> {
        Ok(if self.find_adding(question, tabs, items, add)? {
            Adding::Done
        } else {
            Adding::Unsupported
        })
    }
}

/// How adding things from a list ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Adding {
    Done,
    /// The terminal can't do this; ask in words.
    Unsupported,
    /// A knob was edited, so the list is to be made again with the change.
    Edited(usize, String),
}

/// Asks the person to type something.
pub trait Ask {
    /// The line typed, trimmed; `None` when it is empty or the input ended, which
    /// is how a question is declined. An error when there is nobody to ask.
    fn ask(&self, question: &str) -> Result<Option<String>>;
}

impl Ask for Terminal {
    fn ask(&self, question: &str) -> Result<Option<String>> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!("cannot ask \"{question}\" without a terminal");
        }
        prompt!("{question} > ");
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let line = line.trim();
        Ok((!line.is_empty()).then(|| line.to_string()))
    }
}

/// Asks on the terminal; without one, takes the safe default (or, for a question
/// with no safe default, says there is nobody to ask).
pub struct Terminal;

impl Terminal {
    fn check(question: &str) -> Result<()> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!("cannot ask \"{question}\" without a terminal");
        }
        Ok(())
    }
}

impl Choose for Terminal {
    fn choose(&self, question: &str, options: &Table) -> Result<Option<usize>> {
        Ok(match self.choose_with(question, options, &[])? {
            Choice::Row(i) => Some(i),
            _ => None,
        })
    }

    fn choose_with(&self, question: &str, options: &Table, hotkeys: &[Hotkey]) -> Result<Choice> {
        Self::check(question)?;
        match select::pick(question, options, hotkeys)? {
            select::Picked::Chose(i) => Ok(Choice::Row(i)),
            select::Picked::Key(c, at) => Ok(Choice::Key(c, at)),
            select::Picked::Left => Ok(Choice::Left),
            // A terminal that can't give single keys (`TERM=dumb`): type a number.
            select::Picked::NoRawMode => Ok(
                match choose_from(question, &options.natural(), &mut io::stdin().lock())? {
                    Some(i) => Choice::Row(i),
                    None => Choice::Left,
                },
            ),
        }
    }

    fn view(&self, title: &str, doc: &Doc, hotkeys: &[Hotkey]) -> Result<Option<Choice>> {
        Self::check(title)?;
        Ok(match select::view(title, doc, hotkeys)? {
            select::Picked::Key(c, at) => Some(Choice::Key(c, at)),
            select::Picked::Chose(at) => Some(Choice::Row(at)),
            select::Picked::Left => Some(Choice::Left),
            select::Picked::NoRawMode => None,
        })
    }

    fn find_adding_knobs(
        &self,
        question: &str,
        tabs: &[&str],
        items: &[Item],
        knobs: &[Knob],
        add: &mut dyn FnMut(Pick) -> Result<String, String>,
    ) -> Result<Adding> {
        Self::check(question)?;
        Ok(
            match select::find_adding(question, tabs, items, knobs, add)? {
                select::Added::Done => Adding::Done,
                select::Added::NoRawMode => Adding::Unsupported,
                select::Added::Edited(at, text) => Adding::Edited(at, text),
            },
        )
    }

    fn find_adding(
        &self,
        question: &str,
        tabs: &[&str],
        items: &[Item],
        add: &mut dyn FnMut(Pick) -> Result<String, String>,
    ) -> Result<bool> {
        Ok(matches!(
            self.find_adding_knobs(question, tabs, items, &[], add)?,
            Adding::Done
        ))
    }

    fn find_knobs(
        &self,
        question: &str,
        tabs: &[&str],
        items: &[Item],
        knobs: &[Knob],
    ) -> Result<Selection> {
        Self::check(question)?;
        Ok(match select::find(question, tabs, items, knobs)? {
            select::Found::Chose(items) => Selection::Items(items),
            select::Found::One(i) => Selection::Items(vec![i]),
            select::Found::Path(p) => Selection::Path(p),
            select::Found::Edited(at, text) => Selection::Edited(at, text),
            select::Found::Left => Selection::Left,
            select::Found::NoRawMode => Selection::Unsupported,
        })
    }

    fn find(&self, question: &str, tabs: &[&str], items: &[Item]) -> Result<Selection> {
        self.find_knobs(question, tabs, items, &[])
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
