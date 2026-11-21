//! What lodge reads from the machine it runs on, behind one interface: environment
//! variables, files it was pointed at (a secret's source), and a local shell for a
//! secret's `command`. Nothing else asks the process for them, so a test hands in
//! `fake::FakeLocal` and the real one ([`SystemLocal`]) is built once, in `cmd/`.
//!
//! This is the machine lodge *runs on*. The project's machine, which a `--remote`
//! makes another one, is a [`Host`](crate::host::Host).

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The machine lodge runs on.
pub trait Local: Send + Sync {
    /// An environment variable, `None` when it is unset.
    fn var(&self, name: &str) -> Option<OsString>;

    /// The contents of a file on this machine.
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// Runs `script` with `sh -c` on this machine, with no input and the
    /// terminal for its errors, and returns what it printed. An error when it
    /// exits non-zero or does not finish within `timeout`.
    fn run_sh(&self, script: &str, timeout: Duration) -> Result<Vec<u8>>;
}

/// This machine, for real.
pub struct SystemLocal;

impl Local for SystemLocal {
    fn var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// A secret's source is on the machine running lodge whatever the project's
    /// host is (`pass show x`), so this is deliberately not a
    /// [`Host`](crate::host::Host) command, which runs on the project's host.
    fn run_sh(&self, script: &str, timeout: Duration) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        crate::output::trace(&cmd);
        let mut child = cmd.spawn().context("running sh")?;
        let mut pipe = child.stdout.take().context("capturing stdout")?;
        // Drain on a thread so a chatty child can't block on a full pipe.
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).ok();
            buf
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().context("waiting for command")? {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().ok();
                child.wait().ok();
                reader.join().ok();
                bail!("timed out after {}s", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = reader.join().unwrap_or_default();
        if !status.success() {
            bail!("{status}");
        }
        Ok(stdout)
    }
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

        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::ErrorKind::NotFound.into())
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
