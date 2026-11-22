//! What lodge reads from the machine it runs on, behind one interface: environment
//! variables, files it was pointed at (a secret's source), and a local shell for a
//! secret's `command`. Nothing else asks the process for them, so a test hands in
//! `fake::FakeLocal` and the real one ([`SystemLocal`]) is built once, in `cmd/`.
//!
//! This is the machine lodge *runs on*. The project's machine, which a `--remote`
//! makes another one, is a [`Host`](crate::host::Host).

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The machine lodge runs on.
pub trait Local: Send + Sync {
    /// An environment variable, `None` when it is unset.
    fn var(&self, name: &str) -> Option<OsString>;

    /// The home directory of the user lodge runs as; `/` where there is none, so
    /// that no path is relative to it.
    fn home(&self) -> PathBuf;

    /// The contents of a file on this machine.
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// Runs `script` with this machine's shell (`sh -c`, or `cmd /C` on
    /// Windows), with no input and the
    /// terminal for its errors, and returns what it printed. An error when it
    /// exits non-zero or does not finish within `timeout`.
    fn run_sh(&self, script: &str, timeout: Duration) -> Result<Vec<u8>>;

    /// Runs `program` (found on `PATH`) with `args`, feeds it `input` and reads
    /// what it prints. A program that exits non-zero is an answer
    /// ([`ProgramOutput::success`]), not an error: some say what they mean on
    /// the way out. An error when it cannot be started or does not finish within
    /// `timeout`.
    fn run_program(
        &self,
        program: &str,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<ProgramOutput>;
}

/// What a program printed and whether it succeeded.
// No `Debug`: stdout may hold a secret.
pub struct ProgramOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// This machine, for real.
pub struct SystemLocal;

impl Local for SystemLocal {
    fn var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    /// Where the system says it is (`HOME` on Unix, the user's profile folder on
    /// Windows, where `HOME` is not set), the same answer `~` in the config gets.
    fn home(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// A secret's source is on the machine running lodge whatever the project's
    /// host is (`pass show x`), so this is deliberately not a
    /// [`Host`](crate::host::Host) command, which runs on the project's host.
    fn run_sh(&self, script: &str, timeout: Duration) -> Result<Vec<u8>> {
        let cmd = crate::platform::shell_command(script);
        let out = run(cmd, &[], Stdio::inherit(), timeout)?;
        if !out.success {
            bail!("{}", out.status);
        }
        Ok(out.stdout)
    }

    fn run_program(
        &self,
        program: &str,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<ProgramOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        let out = run(cmd, input, Stdio::piped(), timeout)
            .with_context(|| format!("running {program}"))?;
        Ok(ProgramOutput {
            success: out.success,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

struct Finished {
    success: bool,
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Starts `cmd`, feeds it `input`, and collects its output within `timeout`.
fn run(mut cmd: Command, input: &[u8], stderr: Stdio, timeout: Duration) -> Result<Finished> {
    use std::io::{Read, Write};
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr);
    crate::output::trace(&cmd);
    let mut child = cmd.spawn().context("starting it")?;
    // Written and drained on threads, so a child that fills a pipe or does not
    // read its input cannot block us past the deadline.
    let mut stdin = child.stdin.take().context("opening its input")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        stdin.write_all(&input).ok();
    });
    let mut out_pipe = child.stdout.take().context("capturing stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        out_pipe.read_to_end(&mut buf).ok();
        buf
    });
    let err_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).ok();
            buf
        })
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for command")? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    writer.join().ok();
    Ok(Finished {
        success: status.success(),
        status,
        stdout: reader.join().unwrap_or_default(),
        stderr: err_reader
            .map(|r| r.join().unwrap_or_default())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;

    /// A machine with exactly the variables, files and command outputs it is
    /// given.
    #[derive(Default)]
    pub struct FakeLocal {
        vars: BTreeMap<String, OsString>,
        files: BTreeMap<std::path::PathBuf, Vec<u8>>,
        scripts: BTreeMap<String, std::result::Result<Vec<u8>, String>>,
        programs: BTreeMap<String, (bool, Vec<u8>)>,
        /// What each program was asked, and fed.
        pub asked: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl FakeLocal {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.into(), value.into());
            self
        }

        pub fn with_file(mut self, path: &str, contents: &[u8]) -> Self {
            self.files.insert(path.into(), contents.to_vec());
            self
        }

        /// `script` prints `stdout`.
        pub fn with_script(mut self, script: &str, stdout: &[u8]) -> Self {
            self.scripts.insert(script.into(), Ok(stdout.to_vec()));
            self
        }

        /// `program args…` (joined by spaces) answers with `stdout`, and succeeds or not.
        pub fn with_program(mut self, command: &str, success: bool, stdout: &[u8]) -> Self {
            self.programs
                .insert(command.into(), (success, stdout.to_vec()));
            self
        }

        /// `script` fails with `message`.
        pub fn with_failing_script(mut self, script: &str, message: &str) -> Self {
            self.scripts.insert(script.into(), Err(message.into()));
            self
        }
    }

    impl Local for FakeLocal {
        fn var(&self, name: &str) -> Option<OsString> {
            self.vars.get(name).cloned()
        }

        fn home(&self) -> PathBuf {
            self.var("HOME")
                .map_or_else(|| PathBuf::from("/"), PathBuf::from)
        }

        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::ErrorKind::NotFound.into())
        }

        fn run_program(
            &self,
            program: &str,
            args: &[&str],
            input: &[u8],
            _timeout: Duration,
        ) -> Result<ProgramOutput> {
            let command = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.asked
                .lock()
                .unwrap()
                .push((command.clone(), input.to_vec()));
            match self.programs.get(&command) {
                Some((success, stdout)) => Ok(ProgramOutput {
                    success: *success,
                    stdout: stdout.clone(),
                    stderr: Vec::new(),
                }),
                None => bail!("{program} was not found"),
            }
        }

        fn run_sh(&self, script: &str, _timeout: Duration) -> Result<Vec<u8>> {
            match self.scripts.get(script) {
                Some(Ok(out)) => Ok(out.clone()),
                Some(Err(message)) => bail!("{message}"),
                None => bail!("FakeLocal: unexpected script {script:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written in `sh` and run with `cat`: a Unix machine's.
    #[cfg(unix)]
    #[test]
    fn a_script_runs_prints_fails_and_times_out() {
        let local = SystemLocal;
        let ok = local
            .run_sh("printf '  s3cret\\n'", Duration::from_secs(5))
            .unwrap();
        assert_eq!(ok, b"  s3cret\n");
        let failed = local.run_sh("exit 3", Duration::from_secs(5)).unwrap_err();
        assert!(format!("{failed:#}").contains("exit"), "{failed:#}");
        let slow = local
            .run_sh("sleep 5", Duration::from_millis(100))
            .unwrap_err();
        assert!(slow.to_string().contains("timed out"));
    }

    // Written in `sh` and run with `cat`: a Unix machine's.
    #[cfg(unix)]
    #[test]
    fn a_program_is_fed_its_input_and_a_failure_is_an_answer() {
        let local = SystemLocal;
        let out = local
            .run_program("cat", &[], b"hello", Duration::from_secs(5))
            .unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, b"hello");
        let failed = local
            .run_program(
                "sh",
                &["-c", "echo no >&2; echo out; exit 1"],
                b"",
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(!failed.success);
        assert_eq!(failed.stdout, b"out\n");
        assert_eq!(failed.stderr, b"no\n");
        let missing = local
            .run_program("lodge-no-such-program", &[], b"", Duration::from_secs(5))
            .err()
            .unwrap();
        assert!(
            format!("{missing:#}").contains("lodge-no-such-program"),
            "{missing:#}"
        );
        let slow = local
            .run_program("sleep", &["5"], b"", Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(format!("{slow:#}").contains("timed out"));
    }

    #[test]
    fn files_and_variables_are_this_machines() {
        let local = SystemLocal;
        assert_eq!(local.var("LODGE_DEFINITELY_UNSET_VAR"), None);
        assert!(local.var("PATH").is_some());
        let tmp = crate::tmpfile::TempDir::new("local").unwrap();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        assert_eq!(local.read_file(&tmp.path().join("f")).unwrap(), b"x");
        assert!(local.read_file(&tmp.path().join("g")).is_err());
    }
}
