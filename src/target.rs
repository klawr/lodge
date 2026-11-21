//! Where a project lives: on this machine, or on a remote host reached over SSH.
//!
//! This is the single place that answers "local or remote?". Everything else
//! takes a [`Target`] rather than a bare `--remote` string, so adding a new operation means
//! handling the two cases once, here, instead of in every caller.

use anyhow::{Context, Result};

use crate::output::warning;
use crate::shell::{CmdLine, shell_quote};
use crate::tmpfile::TempFile;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

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

    /// `" on <host>"` for a remote target, nothing for a local one: the tail of
    /// a message about where something was (not) found.
    pub fn on_host(&self) -> String {
        self.ssh_host()
            .map(|h| format!(" on {h}"))
            .unwrap_or_default()
    }

    /// Runs one step against the project's host. Locally that is just `op`;
    /// over ssh a lost connection is retried when the step is `repeat`-safe (see
    /// [`crate::ssh::retry`]), so `op` runs afresh each time.
    pub fn attempt<T: crate::ssh::Outcome>(
        &self,
        repeat: crate::ssh::Repeat,
        mut op: impl FnMut() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        match self {
            Self::Local => op(),
            Self::Ssh(host) => crate::ssh::retry(host, repeat, op),
        }
    }

    /// Runs `sh -c <script>` on the project's host and captures its output.
    /// Over ssh the script is rerun if the connection drops, so it must be safe
    /// to run twice: a read or a probe.
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
    pub fn run_in(&self, dir: &Path, cmd: &CmdLine) -> Result<ExitStatus> {
        let mut command = match self {
            Self::Local => {
                let mut c = match cmd {
                    CmdLine::Shell(script) => {
                        let mut c = Command::new("sh");
                        c.arg("-c").arg(script);
                        c
                    }
                    CmdLine::Argv(argv) => {
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
                    CmdLine::Shell(script) => {
                        format!("sh -c {}", shell_quote(script))
                    }
                    CmdLine::Argv(argv) => argv
                        .iter()
                        .map(|a| shell_quote(a))
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                let script = format!("cd {} && {body}", shell_quote(&dir.to_string_lossy()));
                crate::ssh::command(host, &script)
            }
        };
        crate::output::trace(&command);
        command.status().context("spawning host command")
    }

    /// Writes `content` to a new scratch file on the project's host, private to
    /// the user, which is removed when the returned guard is dropped. `name`
    /// shows what it is for in the local file name.
    pub fn write_scratch(&self, name: &str, content: &str) -> Result<ScratchFile<'_>> {
        match self {
            Self::Local => {
                let file = TempFile::write(name, content.as_bytes())
                    .with_context(|| format!("writing {name}"))?;
                Ok(ScratchFile {
                    target: self,
                    path: file.path().to_string_lossy().into_owned(),
                    local: Some(file),
                })
            }
            Self::Ssh(host) => Ok(ScratchFile {
                target: self,
                path: crate::ssh::write_scratch(host, content)?,
                local: None,
            }),
        }
    }

    /// Removes `path` on the project's host. Best effort: a leftover scratch
    /// file is not worth failing over.
    pub fn remove_file(&self, path: &str) {
        match self {
            Self::Local => crate::tmpfile::discard_file(Path::new(path)),
            Self::Ssh(_) => {
                let script = format!("rm -f {}", shell_quote(path));
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
                let q = shell_quote(&path.to_string_lossy());
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

    /// The folder `dir` on the project's host as an absolute path with every
    /// symlink, `.` and `..` resolved, so one folder has one name however it
    /// was spelled. An error if it doesn't exist. A relative `dir` is relative
    /// to the current directory here, or to the login directory on a remote.
    pub fn canonical_dir(&self, dir: &Path) -> Result<PathBuf> {
        match self {
            Self::Local => std::fs::canonicalize(dir)
                .with_context(|| format!("project folder {} does not exist", dir.display())),
            Self::Ssh(host) => {
                let out = self.sh_output(&canonical_dir_script(dir))?;
                let path = String::from_utf8_lossy(&out.stdout)
                    .trim_end_matches('\n')
                    .to_string();
                if !out.status.success() || !path.starts_with('/') {
                    anyhow::bail!(
                        "project folder {} does not exist on {host}: {}",
                        dir.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(PathBuf::from(path))
            }
        }
    }

    /// The regular files on the project's host that match `patterns`, in the
    /// order of the patterns (and sorted within one), and the contents of the
    /// first of them. A path component that is exactly `*` matches any one
    /// name; nothing else is special. One round trip with `--remote`.
    pub fn read_first(&self, patterns: &[PathBuf]) -> Result<FirstFile> {
        match self {
            Self::Local => {
                let paths: Vec<PathBuf> = patterns.iter().flat_map(|p| expand_local(p)).collect();
                let contents = match paths.first() {
                    Some(first) => Some(
                        std::fs::read(first)
                            .with_context(|| format!("reading {}", first.display()))?,
                    ),
                    None => None,
                };
                Ok(FirstFile { paths, contents })
            }
            Self::Ssh(host) => {
                let out = self.sh_output(&read_first_script(patterns))?;
                if !out.status.success() {
                    anyhow::bail!(
                        "ssh to {host} failed ({}): {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                parse_read_first(&out.stdout)
                    .with_context(|| format!("unexpected answer from {host} listing files"))
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
                let script = format!("tar -C {} -cf - .", shell_quote(&dir.to_string_lossy()));
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
}

/// A file from [`Target::write_scratch`], removed when dropped.
pub struct ScratchFile<'a> {
    target: &'a Target,
    path: String,
    /// The local file, which removes itself; `None` for a remote one.
    local: Option<TempFile>,
}

impl ScratchFile<'_> {
    /// Its path on the project's host.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for ScratchFile<'_> {
    fn drop(&mut self) {
        if self.local.is_none() {
            self.target.remove_file(&self.path);
        }
    }
}

/// The `sh` script behind a remote [`Target::canonical_dir`].
fn canonical_dir_script(dir: &Path) -> String {
    format!("cd -- {} && pwd -P", shell_quote(&dir.to_string_lossy()))
}

/// What [`Target::read_first`] found.
#[derive(Debug, PartialEq, Eq)]
pub struct FirstFile {
    /// Every match, in order.
    pub paths: Vec<PathBuf>,
    /// The contents of the first match, if there is one.
    pub contents: Option<Vec<u8>>,
}

/// The `sh` script behind a remote [`Target::read_first`]: each match ends with a
/// NUL, an extra NUL ends the list (paths are never empty), then the first
/// match's bytes follow. Literal path components are quoted; a `*` component is
/// left bare for the shell to expand.
fn read_first_script(patterns: &[PathBuf]) -> String {
    let words: Vec<String> = patterns
        .iter()
        .map(|p| {
            p.to_string_lossy()
                .split('/')
                .map(|c| match c {
                    "*" | "" => c.to_string(),
                    c => shell_quote(c),
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    format!(
        "first=; for f in {}; do [ -f \"$f\" ] || continue; printf '%s\\0' \"$f\"; \
         [ -n \"$first\" ] || first=$f; done; printf '\\0'; \
         [ -z \"$first\" ] || cat -- \"$first\"",
        words.join(" ")
    )
}

fn parse_read_first(out: &[u8]) -> Option<FirstFile> {
    let end = out
        .windows(2)
        .position(|w| w == [0, 0])
        .map(|i| i + 1)
        .or_else(|| {
            // No matches: the list is just the terminating NUL.
            (out.first() == Some(&0)).then_some(0)
        })?;
    let paths: Vec<PathBuf> = out[..end]
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(String::from_utf8_lossy(p).as_ref()))
        .collect();
    let contents = (!paths.is_empty()).then(|| out[end + 1..].to_vec());
    Some(FirstFile { paths, contents })
}

/// The local files matching `pattern` (see [`Target::read_first`]), sorted.
fn expand_local(pattern: &Path) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for component in pattern.components() {
        let name = component.as_os_str();
        found = if name == "*" {
            let mut next: Vec<PathBuf> = found
                .iter()
                .filter_map(|dir| std::fs::read_dir(dir).ok())
                .flatten()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            next.sort();
            next
        } else {
            found.into_iter().map(|p| p.join(name)).collect()
        };
    }
    found.retain(|p| p.is_file());
    found
}

#[cfg(test)]
mod tests {
    use super::Target;
    use std::path::PathBuf;

    #[test]
    fn the_remote_script_finds_and_reads_like_the_local_lookup() {
        // The script, run for real by `sh` here, against the same tree.
        let tmp = crate::tmpfile::TempDir::new("read-first").unwrap();
        let p = tmp.path().join("it's here");
        for (f, body) in [
            ("a.json", "A"),
            ("sub/b/x.json", "B"),
            ("sub/a/x.json", "AA"),
        ] {
            std::fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            std::fs::write(p.join(f), body).unwrap();
        }
        let run = |patterns: &[PathBuf]| {
            let out = std::process::Command::new("sh")
                .args(["-c", &super::read_first_script(patterns)])
                .output()
                .unwrap();
            assert!(out.status.success());
            super::parse_read_first(&out.stdout).unwrap()
        };
        let patterns = [p.join("missing"), p.join("sub/*/x.json"), p.join("a.json")];
        assert_eq!(run(&patterns), Target::Local.read_first(&patterns).unwrap());
        assert_eq!(run(&patterns).contents.as_deref(), Some(&b"AA"[..]));
        let none = [p.join("nope"), p.join("*/nope")];
        assert_eq!(run(&none), Target::Local.read_first(&none).unwrap());
        assert!(run(&none).paths.is_empty());
    }

    #[test]
    fn matches_follow_the_pattern_order_and_expand_stars() {
        let tmp = crate::tmpfile::TempDir::new("existing").unwrap();
        let p = tmp.path();
        for f in ["a.json", "sub/b/x.json", "sub/a/x.json", "sub/c/other"] {
            std::fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            std::fs::write(p.join(f), "").unwrap();
        }
        let found = Target::Local
            .read_first(&[p.join("missing"), p.join("sub/*/x.json"), p.join("a.json")])
            .unwrap();
        assert_eq!(
            found.paths,
            [
                p.join("sub/a/x.json"),
                p.join("sub/b/x.json"),
                p.join("a.json")
            ]
        );
    }

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
    fn the_remote_canonical_dir_script_resolves_like_the_local_lookup() {
        let tmp = crate::tmpfile::TempDir::new("canon").unwrap();
        std::fs::create_dir_all(tmp.path().join("a b/c")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a b"), tmp.path().join("link")).unwrap();
        let spelled = tmp.path().join("link/c/../c/.");
        let out = std::process::Command::new("sh")
            .args(["-c", &super::canonical_dir_script(&spelled)])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end()),
            Target::Local.canonical_dir(&spelled).unwrap()
        );
        assert!(
            Target::Local
                .canonical_dir(&tmp.path().join("missing"))
                .is_err()
        );
    }

    #[test]
    fn a_local_scratch_file_holds_the_content_until_dropped() {
        let f = Target::Local.write_scratch("x.yml", "a: 1\n").unwrap();
        let path = std::path::PathBuf::from(f.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a: 1\n");
        drop(f);
        assert!(!path.exists());
    }

    #[test]
    fn the_remote_scratch_script_makes_a_private_file_and_prints_it() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::tmpfile::TempDir::new("scratch").unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", crate::ssh::WRITE_SCRATCH_SCRIPT])
            .env("TMPDIR", dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"services: {}\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let path = String::from_utf8(out.stdout).unwrap();
        assert!(
            path.starts_with(&dir.path().to_string_lossy().into_owned()),
            "{path}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "services: {}\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
