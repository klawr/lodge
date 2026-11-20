//! Where a project lives: on this machine, or on a remote host reached over SSH.
//!
//! This is the single place that answers "local or remote?". Everything else
//! takes a [`Target`] (or a [`Runtime`](crate::runtime::Runtime), which holds one)
//! rather than a bare `--remote` string, so adding a new operation means
//! handling the two cases once, here, instead of in every caller.

use anyhow::{Context, Result};

use crate::output::warning;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// A command to run on the project's host.
pub enum HostCmd<'a> {
    /// A script for `sh -c`.
    Shell(&'a str),
    /// A program and its arguments, run directly.
    Argv(&'a [String]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The local machine and its Docker daemon.
    Local,
    /// A remote host (the `--remote` value, as `ssh` understands it). The docker
    /// CLI runs *on* that host, so project files must be read there too.
    Ssh(String),
}

impl Target {
    /// Builds a target from the `--remote` flag.
    pub fn new(remote: Option<String>) -> Self {
        match remote {
            Some(host) => Self::Ssh(host),
            None => Self::Local,
        }
    }

    /// The `--remote` destination, if this target is remote.
    pub fn ssh_host(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Ssh(host) => Some(host),
        }
    }

    /// Runs `sh -c <script>` on the project's host and captures its output.
    pub fn sh_output(&self, script: &str) -> Result<std::process::Output> {
        match self {
            Self::Local => {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", script]);
                crate::output::trace(&cmd);
                cmd.output().context("running sh")
            }
            Self::Ssh(host) => crate::ssh::sh_output(host, script),
        }
    }

    /// Runs `cmd` with `dir` as its working directory on the project's host,
    /// with inherited stdio.
    pub fn run_in(&self, dir: &Path, cmd: &HostCmd) -> Result<ExitStatus> {
        let mut command = match self {
            Self::Local => {
                let mut c = match cmd {
                    HostCmd::Shell(script) => {
                        let mut c = Command::new("sh");
                        c.arg("-c").arg(script);
                        c
                    }
                    HostCmd::Argv(argv) => {
                        let (program, args) = argv.split_first().context("empty command")?;
                        let mut c = Command::new(program);
                        c.args(args);
                        c
                    }
                };
                c.current_dir(dir);
                c
            }
            Self::Ssh(host) => {
                // Force `sh` semantics for the shell form rather than running
                // the string in the remote user's login shell.
                let body = match cmd {
                    HostCmd::Shell(script) => {
                        format!("sh -c {}", crate::shell::shell_quote(script))
                    }
                    HostCmd::Argv(argv) => argv
                        .iter()
                        .map(|a| crate::shell::shell_quote(a))
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                let script = format!(
                    "cd {} && {body}",
                    crate::shell::shell_quote(&dir.to_string_lossy())
                );
                crate::ssh::command(host, &script)
            }
        };
        crate::output::trace(&command);
        command.status().context("spawning host command")
    }

    /// Writes `content` to `path` on the host the project lives on.
    pub fn write_file(&self, path: &str, content: &str) -> Result<()> {
        match self {
            Self::Local => std::fs::write(path, content).with_context(|| format!("writing {path}")),
            Self::Ssh(host) => crate::ssh::write_file(host, path, content),
        }
    }

    /// Removes `path` on the project's host. Best effort: a leftover scratch
    /// file is not worth failing over.
    pub fn remove_file(&self, path: &str) {
        match self {
            Self::Local => crate::tmpfile::discard_file(Path::new(path)),
            Self::Ssh(_) => {
                let script = format!("rm -f {}", crate::shell::shell_quote(path));
                if let Err(e) = self.sh_output(&script) {
                    warning!("could not remove {path} on the remote host: {e:#}");
                }
            }
        }
    }

    /// The contents of the file `path` on the project's host, or `None` if
    /// there is no such file.
    pub fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Local => match std::fs::read(path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            },
            Self::Ssh(host) => {
                let q = crate::shell::shell_quote(&path.to_string_lossy());
                // Exit 3 means "no such file", told apart from ssh's own failures.
                let out = crate::ssh::sh_output(host, &format!("[ -f {q} ] || exit 3; cat {q}"))?;
                match out.status.code() {
                    Some(0) => Ok(Some(out.stdout)),
                    Some(3) => Ok(None),
                    _ => anyhow::bail!(
                        "reading {} on {host} failed: {}",
                        path.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                }
            }
        }
    }

    /// Makes the directory `dir` on the project's host readable here. Locally
    /// that is `dir` itself; remotely its contents are copied into `dest` (one
    /// `tar` stream over ssh, unpacked in-process) and `dest` is returned.
    pub fn fetch_dir(&self, dir: &Path, dest: &Path) -> Result<std::path::PathBuf> {
        match self {
            Self::Local => Ok(dir.to_path_buf()),
            Self::Ssh(host) => {
                let script = format!(
                    "tar -C {} -cf - .",
                    crate::shell::shell_quote(&dir.to_string_lossy())
                );
                let out = crate::ssh::sh_output(host, &script)?;
                if !out.status.success() {
                    anyhow::bail!(
                        "reading {} on {host} failed: {}",
                        dir.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                std::fs::create_dir_all(dest)?;
                tar::Archive::new(out.stdout.as_slice())
                    .unpack(dest)
                    .with_context(|| format!("unpacking {} from {host}", dir.display()))?;
                Ok(dest.to_path_buf())
            }
        }
    }

    /// The uid and gid of the user lodge runs as on the project's host: this
    /// machine, or the remote one (that is who owns a bind-mounted workspace).
    pub fn uid_gid(&self) -> Result<(u32, u32)> {
        let out = self.sh_output("id -u; id -g")?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut ids = stdout.lines().map(|l| l.trim().parse::<u32>());
        match (ids.next(), ids.next()) {
            (Some(Ok(uid)), Some(Ok(gid))) => Ok((uid, gid)),
            _ => anyhow::bail!("could not read the host user's uid/gid (got {stdout:?})"),
        }
    }

    /// A per-process scratch path for `name` on the project's host: the system
    /// temp dir locally, `/tmp` remotely.
    pub fn scratch_path(&self, name: &str) -> String {
        let file = format!("lodge-{}-{name}", std::process::id());
        match self {
            Self::Local => std::env::temp_dir()
                .join(file)
                .to_string_lossy()
                .into_owned(),
            Self::Ssh(_) => format!("/tmp/{file}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Target;

    #[test]
    fn from_flag() {
        assert_eq!(Target::new(None), Target::Local);
        assert_eq!(Target::new(Some("h".into())), Target::Ssh("h".into()));
    }

    #[test]
    fn ssh_host_only_for_remote() {
        assert_eq!(Target::Local.ssh_host(), None);
        assert_eq!(Target::Ssh("u@h:22".into()).ssh_host(), Some("u@h:22"));
    }

    #[test]
    fn remote_scratch_is_under_tmp() {
        assert!(
            Target::Ssh("h".into())
                .scratch_path("x")
                .starts_with("/tmp/lodge-")
        );
    }
}
