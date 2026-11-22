//! Everything lodge says to the user, and the verbosity setting.
//!
//! Messages come in five kinds, each with a macro, all on stderr:
//!
//! - `info!`: progress.
//! - `note!`: a hint, or the context of a question, tagged `note:`.
//! - `warning!`: something went wrong but lodge carries on, tagged `warning:`.
//! - `error!`: why a command failed, tagged `error:`.
//! - `prompt!`: a question, without a line break, flushed.
//!
//! Reports and lists (`status`, `ls`, `--json`) are program output and use
//! `line!`, without the prefix, on stdout. Everything else goes to stderr, so
//! stdout carries only what a script would want to read. The rest are for
//! special cases: `line_err!` (an unprefixed line on stderr, such as the
//! choices of a question), `scoped!` (a message from a named part of lodge,
//! such as the forward daemon's log), and `raw_stdout` / `raw_stderr` (text
//! passed through untouched: a generated file, a log, another program's
//! output). `trace` and `trace_line` show what lodge runs, under `--verbose`.
//!
//! Every line carries the `[lodge]` prefix, including the continuation lines of
//! a multi-line message, so call sites write plain text. Verbosity is set once
//! at startup from `--verbose` and read by `trace` and by the ssh helpers. It
//! is process-wide state, like the level of a logging crate, rather than a
//! parameter threaded through every function that builds a command.

// Writing to a closed terminal (a broken pipe) is not worth aborting over, and
// there is nobody left to tell, so write results are ignored in this module.
#![allow(clippy::let_underscore_must_use)]

use std::fmt;
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

static VERBOSITY: AtomicU8 = AtomicU8::new(0);
static WARNINGS: AtomicUsize = AtomicUsize::new(0);

/// How many warnings have been printed so far: what a screen that covers the
/// terminal compares before and after, to know whether there is something to
/// read before it covers it again.
pub fn warnings() -> usize {
    WARNINGS.load(Ordering::Relaxed)
}

/// Sets the verbosity from the number of `-v` flags.
pub fn set_verbosity(level: u8) {
    VERBOSITY.store(level, Ordering::Relaxed);
}

pub fn verbosity() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}

pub fn is_verbose() -> bool {
    verbosity() > 0
}

/// Renders `text` with the scope prefix on every line, and `tag` (such as
/// `warning: `) on the first.
fn render(scope: &str, tag: &str, text: &str) -> String {
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let tag = if i == 0 { tag } else { "" };
            format!("[{scope}] {tag}{line}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Writes a prefixed message to stderr, where everything but program output
/// goes.
fn emit(scope: &str, tag: &str, args: fmt::Arguments) {
    let rendered = render(scope, tag, &args.to_string());
    // A failed write to a closed stderr (a broken pipe) is not worth aborting
    // over; there is nobody left to tell.
    let _ = writeln!(std::io::stderr(), "{rendered}");
}

pub fn print_info(args: fmt::Arguments) {
    emit("lodge", "", args);
}

pub fn print_warn(args: fmt::Arguments) {
    WARNINGS.fetch_add(1, Ordering::Relaxed);
    emit("lodge", "warning: ", args);
}

/// Why lodge stopped: the error a command failed with.
pub fn print_error(args: fmt::Arguments) {
    emit("lodge", "error: ", args);
}

pub fn print_note(args: fmt::Arguments) {
    emit("lodge", "note: ", args);
}

/// A message from a named part of lodge, such as the port-forward daemon, whose
/// stderr is a log file.
pub fn print_scoped(scope: &str, args: fmt::Arguments) {
    emit(scope, "", args);
}

/// A line of program output (a report, a list), without the `[lodge]` prefix.
pub fn print_line(args: fmt::Arguments) {
    let _ = writeln!(std::io::stdout(), "{args}");
}

/// A line of program output on stderr, without the prefix.
pub fn print_line_err(args: fmt::Arguments) {
    let _ = writeln!(std::io::stderr(), "{args}");
}

/// Writes text to stdout untouched (a generated file, a log being shown).
pub fn raw_stdout(text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "{text}");
    let _ = out.flush();
}

/// Passes text through to stderr untouched: another program's own output, such
/// as ssh's debug chatter under `-v`.
pub fn raw_stderr(text: &str) {
    let _ = write!(std::io::stderr(), "{text}");
}

/// Prints a prompt on stderr (beside the context `note!` gave it), with the
/// prefix and without a trailing newline, and flushes it so it is visible
/// before input is read.
pub fn print_prompt(args: fmt::Arguments) {
    let mut err = std::io::stderr();
    let _ = write!(err, "{}", render("lodge", "", &args.to_string()));
    let _ = err.flush();
}

/// "1 library", "3 libraries": a count with the right form of its noun.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Prints a command line lodge runs (already in shell form) to stderr. No-op
/// unless `--verbose` is set.
pub fn trace_line(line: &str) {
    if is_verbose() {
        emit("lodge", "$ ", format_args!("{line}"));
    }
}

/// Prints the command (with any explicitly-set env vars) to stderr in
/// shell-pasteable form. No-op unless `--verbose` is set.
pub fn trace(cmd: &Command) {
    if !is_verbose() {
        return;
    }
    let mut line = String::new();
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            line.push_str(&format!(
                "{}={} ",
                k.to_string_lossy(),
                quote(&v.to_string_lossy())
            ));
        }
    }
    line.push_str(&cmd.get_program().to_string_lossy());
    for a in cmd.get_args() {
        line.push(' ');
        line.push_str(&quote(&a.to_string_lossy()));
    }
    trace_line(&line);
}

// Quote only when needed, so typical traces stay readable.
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | ',')
        });
    if plain {
        s.to_string()
    } else {
        crate::shell::shell_quote(s)
    }
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::output::print_info(format_args!($($arg)*)) };
}
macro_rules! warning {
    ($($arg:tt)*) => { $crate::output::print_warn(format_args!($($arg)*)) };
}
macro_rules! note {
    ($($arg:tt)*) => { $crate::output::print_note(format_args!($($arg)*)) };
}
macro_rules! error {
    ($($arg:tt)*) => { $crate::output::print_error(format_args!($($arg)*)) };
}
macro_rules! line {
    ($($arg:tt)*) => { $crate::output::print_line(format_args!($($arg)*)) };
}
macro_rules! line_err {
    ($($arg:tt)*) => { $crate::output::print_line_err(format_args!($($arg)*)) };
}
macro_rules! prompt {
    ($($arg:tt)*) => { $crate::output::print_prompt(format_args!($($arg)*)) };
}
macro_rules! scoped {
    ($scope:expr, $($arg:tt)*) => { $crate::output::print_scoped($scope, format_args!($($arg)*)) };
}
pub(crate) use {error, info, line, line_err, note, prompt, scoped, warning};

#[cfg(test)]
mod tests {
    use super::{quote, render};

    #[test]
    fn every_line_gets_the_prefix_and_only_the_first_gets_the_tag() {
        assert_eq!(render("lodge", "", "one"), "[lodge] one");
        assert_eq!(
            render("lodge", "warning: ", "first\nsecond\nthird"),
            "[lodge] warning: first\n[lodge] second\n[lodge] third"
        );
    }

    #[test]
    fn scopes_name_the_part_that_spoke() {
        assert_eq!(
            render("lodge forward", "", "ready"),
            "[lodge forward] ready"
        );
    }

    #[test]
    fn counts_take_the_right_noun() {
        assert_eq!(super::plural(1, "library", "libraries"), "1 library");
        assert_eq!(super::plural(0, "alias", "aliases"), "0 aliases");
    }

    #[test]
    fn plain_args_stay_unquoted() {
        assert_eq!(quote("docker"), "docker");
        assert_eq!(quote("ssh://user@host:22"), "ssh://user@host:22");
    }

    #[test]
    fn args_with_specials_are_quoted() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote(""), "''");
    }
}
