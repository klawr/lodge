//! An in-memory [`Env`] for tests: a virtual filesystem plus a record of every
//! command that was run. Lets injection logic be tested without a container.
//!
//! It is deliberately strict. It emulates a small set of commands (`cat`, `cp`,
//! `test`, `printenv`, `command -v`, [`Env::read_file`], and the no-output file
//! operations), and a test declares anything else it depends on
//! (`with_output`, `with_failure`, `with_commands`). A command it neither
//! emulates nor was told about **panics**, naming the command, so a test can't
//! pass by accident because the fake answered "success" to something the real
//! container might refuse.
//!
//! One exception: a `sh -c` script run for its effect (with `exec`, not
//! captured) is recorded and reported as succeeded, since its effect can't be
//! emulated. Tests of such scripts assert on the recorded script and on who it
//! ran as, and scripts whose behaviour matters are also run for real against a
//! temp directory (see `secret/place.rs`).

use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;

use super::{Env, ExecOpts, Output, User};
use crate::target::Target;

/// Who a recorded command ran as ([`User`] without the borrow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Default,
    Root,
    Named(String),
}

impl From<User<'_>> for Who {
    fn from(user: User) -> Self {
        match user {
            User::Default => Who::Default,
            User::Root => Who::Root,
            User::Named(n) => Who::Named(n.to_string()),
        }
    }
}

pub struct FakeEnv {
    files: RefCell<HashMap<String, Vec<u8>>>,
    vars: HashMap<String, String>,
    canned: HashMap<Vec<String>, String>,
    failing: Vec<Vec<String>>,
    commands: Vec<String>,
    modes: RefCell<HashMap<String, u32>>,
    /// Every command run, with whom it ran as.
    calls: RefCell<Vec<(Vec<String>, Who)>>,
    target: Target,
}

impl FakeEnv {
    pub fn new() -> Self {
        Self {
            files: RefCell::default(),
            vars: HashMap::new(),
            canned: HashMap::new(),
            failing: Vec::new(),
            commands: Vec::new(),
            modes: RefCell::default(),
            calls: RefCell::default(),
            target: Target::Local,
        }
    }

    /// Sets an environment variable visible to `printenv`.
    pub fn with_var(mut self, name: &str, value: &str) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// Makes the exact command `argv` print `stdout` and succeed.
    pub fn with_output(mut self, argv: &[&str], stdout: &str) -> Self {
        self.canned
            .insert(argv.iter().map(|s| s.to_string()).collect(), stdout.into());
        self
    }

    /// Makes the exact command `argv` exit non-zero.
    pub fn with_failure(mut self, argv: &[&str]) -> Self {
        self.failing
            .push(argv.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Declares which programs exist in the container, for `command -v` checks.
    pub fn with_commands(mut self, names: &[&str]) -> Self {
        self.commands.extend(names.iter().map(|n| n.to_string()));
        self
    }

    /// Pre-populates a file.
    pub fn with_file(self, path: &str, contents: &str) -> Self {
        self.files
            .borrow_mut()
            .insert(path.into(), contents.as_bytes().to_vec());
        self
    }

    /// The text of a file, if it exists.
    pub fn file(&self, path: &str) -> Option<String> {
        self.files
            .borrow()
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// The permission bits a file was last written with by `write_secret`.
    pub fn mode(&self, path: &str) -> Option<u32> {
        self.modes.borrow().get(path).copied()
    }

    /// True if exactly this command was run (as `exec` or `output`).
    pub fn ran(&self, argv: &[&str]) -> bool {
        self.calls
            .borrow()
            .iter()
            .any(|(c, _)| c.iter().map(String::as_str).eq(argv.iter().copied()))
    }

    /// Who exactly this command ran as, if it ran.
    pub fn ran_as(&self, argv: &[&str]) -> Option<Who> {
        self.calls
            .borrow()
            .iter()
            .find(|(c, _)| c.iter().map(String::as_str).eq(argv.iter().copied()))
            .map(|(_, who)| who.clone())
    }

    /// True if any command run so far satisfies `pred`.
    pub fn ran_matching(&self, pred: impl Fn(&[String]) -> bool) -> bool {
        self.calls.borrow().iter().any(|(c, _)| pred(c))
    }

    /// Runs one command against the fake. `capture` is whether the caller wants
    /// its output (as opposed to just running it).
    fn handle(&self, argv: &[&str], user: User, capture: bool) -> Output {
        self.record(argv, user);
        let ok = |stdout: String| Output {
            success: true,
            stdout,
            stderr: String::new(),
        };
        let fail = || Output {
            success: false,
            stdout: String::new(),
            stderr: String::new(),
        };
        let key: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        if self.failing.contains(&key) {
            return fail();
        }
        if let Some(stdout) = self.canned.get(&key) {
            return ok(stdout.clone());
        }
        match argv {
            ["cat", path] => match self.file(path) {
                Some(text) => ok(text),
                None => fail(),
            },
            ["cp", src, dst] => {
                let bytes = self.files.borrow().get(*src).cloned();
                match bytes {
                    Some(b) => {
                        self.files.borrow_mut().insert((*dst).into(), b);
                        ok(String::new())
                    }
                    None => fail(),
                }
            }
            ["test", "-f" | "-e", path] => {
                if self.files.borrow().contains_key(*path) {
                    ok(String::new())
                } else {
                    fail()
                }
            }
            ["sh", "-c", script, path] if *script == super::READ_FILE_SCRIPT => {
                match self.file(path) {
                    Some(text) => ok(format!("y{text}")),
                    None => ok("n".into()),
                }
            }
            ["printenv", name] => match self.vars.get(*name) {
                Some(v) => ok(format!("{v}\n")),
                None => fail(),
            },
            ["rm", .., path] => {
                self.files.borrow_mut().remove(*path);
                ok(String::new())
            }
            // File operations with nothing to report.
            ["mkdir" | "chmod" | "chown" | "chgrp", ..] => ok(String::new()),
            // `command -v <name> …`, as `Env::has_command` runs it.
            ["sh", "-c", script] if script.starts_with("command -v ") => {
                let name = script["command -v ".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches('\'');
                if self.commands.iter().any(|c| c == name) {
                    ok(String::new())
                } else {
                    fail()
                }
            }
            // A script run for its effect (as root, say). Its effect can't be
            // emulated, so it is recorded and assumed to work; a test that cares
            // asserts on the recorded script.
            ["sh", "-c", _, ..] if !capture => ok(String::new()),
            _ => panic!(
                "FakeEnv: unexpected command {argv:?}; declare it with \
                 with_output / with_failure / with_commands"
            ),
        }
    }

    fn record(&self, argv: &[&str], user: User) {
        self.calls
            .borrow_mut()
            .push((argv.iter().map(|s| s.to_string()).collect(), user.into()));
    }
}

impl Env for FakeEnv {
    fn target(&self) -> &Target {
        &self.target
    }

    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        let out = self.handle(argv, opts.user, false);
        if out.success {
            Ok(())
        } else {
            anyhow::bail!("fake exec failed: {argv:?}")
        }
    }

    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        Ok(self.handle(argv, opts.user, true))
    }

    fn cp_in(&self, src: &str, dest: &str) -> Result<()> {
        let bytes = std::fs::read(src).with_context(|| format!("reading {src}"))?;
        self.files.borrow_mut().insert(dest.into(), bytes);
        Ok(())
    }

    // Records the destination and mode, never the value (as the real one keeps
    // it out of argv).
    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        self.record(&["write_secret", dest, &format!("{mode:o}")], user);
        self.files
            .borrow_mut()
            .insert(dest.into(), contents.to_vec());
        self.modes.borrow_mut().insert(dest.into(), mode);
        Ok(())
    }

    fn interactive_command(&self, _user: User, workdir: &str, inner: &str) -> String {
        format!("fake-shell {workdir} {inner}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Env;

    #[test]
    #[should_panic(expected = "FakeEnv: unexpected command")]
    fn an_undeclared_command_panics_instead_of_succeeding() {
        let env = FakeEnv::new();
        let _ = env.exec_capture(&["some-tool", "--flag"]);
    }

    #[test]
    fn declared_commands_answer_and_undeclared_programs_are_absent() {
        let env = FakeEnv::new().with_commands(&["git"]);
        assert!(env.has_command("git"));
        assert!(!env.has_command("sed"));
    }

    #[test]
    fn declared_output_and_failures_are_returned() {
        let env = FakeEnv::new()
            .with_output(&["uname", "-m"], "x86_64\n")
            .with_failure(&["infocmp", "nope"]);
        assert_eq!(env.exec_capture(&["uname", "-m"]).unwrap(), "x86_64\n");
        assert!(!env.succeeds(&["infocmp", "nope"]));
    }

    #[test]
    fn write_secret_stores_bytes_and_mode_but_records_no_value() {
        let env = FakeEnv::new();
        env.write_secret("/dev/shm/s/TOKEN", b"hunter2", 0o600, User::Default)
            .unwrap();
        assert_eq!(env.file("/dev/shm/s/TOKEN").as_deref(), Some("hunter2"));
        assert_eq!(env.mode("/dev/shm/s/TOKEN"), Some(0o600));
        assert!(env.ran(&["write_secret", "/dev/shm/s/TOKEN", "600"]));
        assert!(!env.ran_matching(|c| c.iter().any(|a| a.contains("hunter2"))));
    }

    #[test]
    fn files_written_are_visible_to_later_commands() {
        let env = FakeEnv::new();
        env.write_file("/etc/x", b"hello").unwrap();
        assert!(env.file_exists("/etc/x"));
        assert_eq!(env.exec_capture(&["cat", "/etc/x"]).unwrap(), "hello");
        env.exec_root(&["rm", "-f", "/etc/x"]).unwrap();
        assert!(!env.file_exists("/etc/x"));
    }
}
