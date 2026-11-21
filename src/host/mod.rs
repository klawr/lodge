//! Hosts: the machines lodge runs commands on and moves bytes to.
//!
//! [`Host`] is the one seam between lodge and *how a machine is reached*. The
//! container engine, the devcontainer code, the forwarder and the session all
//! take a host and use only what the trait offers, so they neither know nor care
//! whether it is this machine ([`LocalHost`]), another one over the system `ssh`
//! ([`SshHost`]) or something that does not exist yet. A new way of reaching a
//! machine is a new implementation, handed in where the command layer builds the
//! rest (`cmd/`); no other file changes.
//!
//! A host has three small primitives: [`Host::run`] (one [`Job`] on the host,
//! with the streams the caller asks for), [`Host::interactive`] (a command with a
//! terminal) and [`Host::tunnel`]. No process type crosses the seam, so a host
//! need not be a local process at all, and `fake::FakeHost` stands in for one in
//! tests. Everything else here (running a script, reading a file, resolving a
//! folder, making a scratch file) is provided on top of them, so an
//! implementation gets it for free and overrides only what it can do better (the
//! local host reads files directly).
//!
//! Hosts that can lose their connection say so ([`Host::link_lost`]) and
//! [`attempt`] reruns a step that is [`Repeat::Safe`]; see the architecture doc.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crate::shell::{CmdLine, shell_quote};

#[cfg(test)]
pub mod fake;
mod local;
mod ssh;

pub use local::LocalHost;
pub use ssh::SshHost;

/// A shared handle to a host: what the engine and the other components keep.
pub type HostRef = Arc<dyn Host>;

/// A [`fake::FakeHost`] with nothing on it, as a [`HostRef`], for tests that only
/// need a host to hand over.
#[cfg(test)]
pub(crate) fn fake() -> HostRef {
    Arc::new(fake::FakeHost::new())
}

/// Whether running a step a second time, after the connection dropped while it
/// ran, does no harm. A dropped connection says nothing about whether the remote
/// command ran: it may have finished with only its reply lost. Only a
/// [`Repeat::Safe`] step is rerun; the rest report the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    /// Reads, probes and steps that leave the same state when run again
    /// (`mkdir -p`, `rm -rf`, a file replaced whole).
    Safe,
    /// Everything else: user hooks, installers, anything that creates.
    #[default]
    Never,
}

/// What a finished step tells [`Host::link_lost`].
pub trait Outcome {
    fn status(&self) -> ExitStatus;
    /// What the step printed on stderr, if it was captured.
    fn stderr(&self) -> &[u8] {
        &[]
    }
}

impl Outcome for ExitStatus {
    fn status(&self) -> ExitStatus {
        *self
    }
}

impl Outcome for std::process::Output {
    fn status(&self) -> ExitStatus {
        self.status
    }

    fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

impl Outcome for Ran {
    fn status(&self) -> ExitStatus {
        self.status
    }

    fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

/// A step that also sent data, with how sending went.
impl<T> Outcome for (ExitStatus, T) {
    fn status(&self) -> ExitStatus {
        self.0
    }
}

/// What a stream of a [`Job`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The terminal lodge runs on.
    Inherit,
    /// Collected into the [`Ran`].
    Capture,
    /// Discarded.
    Null,
}

/// What a [`Job`] reads on its stdin.
pub enum Input<'a> {
    /// Nothing: it is closed at once.
    Null,
    /// The terminal lodge runs on.
    Inherit,
    /// What the caller writes: called afresh on each try, and the pipe is closed
    /// when it returns, so the job sees the end of its input.
    Feed(&'a mut dyn FnMut(&mut dyn Write) -> std::io::Result<()>),
}

/// What a [`Job`] runs, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A script for `sh -c`.
    Shell(String),
    /// A program and its arguments, run directly.
    Argv(Vec<String>),
}

impl From<&CmdLine<'_>> for Line {
    fn from(line: &CmdLine<'_>) -> Self {
        match line {
            CmdLine::Shell(script) => Line::Shell((*script).to_string()),
            CmdLine::Argv(argv) => Line::Argv(argv.to_vec()),
        }
    }
}

/// One command to run on a host, and what to do with its streams. The defaults
/// are the quiet ones: no input, output and errors on the terminal, not safe to
/// repeat.
pub struct Job<'a> {
    pub line: Line,
    /// The working directory on the host.
    pub dir: Option<&'a Path>,
    pub stdin: Input<'a>,
    pub stdout: Stream,
    pub stderr: Stream,
    pub repeat: Repeat,
}

impl<'a> Job<'a> {
    pub fn new(line: Line) -> Self {
        Job {
            line,
            dir: None,
            stdin: Input::Null,
            stdout: Stream::Inherit,
            stderr: Stream::Inherit,
            repeat: Repeat::Never,
        }
    }

    /// A program and its arguments.
    pub fn argv(argv: &[&str]) -> Self {
        Self::new(Line::Argv(argv.iter().map(|a| a.to_string()).collect()))
    }

    /// A script for `sh -c`.
    pub fn shell(script: &str) -> Self {
        Self::new(Line::Shell(script.to_string()))
    }

    pub fn in_dir(mut self, dir: Option<&'a Path>) -> Self {
        self.dir = dir;
        self
    }

    /// Collect both output streams.
    pub fn capture(mut self) -> Self {
        self.stdout = Stream::Capture;
        self.stderr = Stream::Capture;
        self
    }

    pub fn stdin(mut self, stdin: Input<'a>) -> Self {
        self.stdin = stdin;
        self
    }

    pub fn stdout(mut self, stdout: Stream) -> Self {
        self.stdout = stdout;
        self
    }

    pub fn stderr(mut self, stderr: Stream) -> Self {
        self.stderr = stderr;
        self
    }

    pub fn repeat(mut self, repeat: Repeat) -> Self {
        self.repeat = repeat;
        self
    }
}

/// A finished [`Job`].
#[derive(Debug)]
pub struct Ran {
    pub status: ExitStatus,
    /// Empty unless the stream was [`Stream::Capture`].
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// How writing the [`Input::Feed`] went (`Ok` when there was none).
    pub fed: std::io::Result<()>,
}

impl Ran {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// A process the host started that keeps running: a tunnel.
pub trait Child: Send {
    /// Stops it. Not an error when it has already exited.
    fn kill(&mut self) -> std::io::Result<()>;

    /// Its exit status if it has exited, `None` while it runs.
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;
}

struct LocalChild(std::process::Child);

impl Child for LocalChild {
    fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }
}

fn stdio(stream: Stream) -> Stdio {
    match stream {
        Stream::Inherit => Stdio::inherit(),
        Stream::Capture => Stdio::piped(),
        Stream::Null => Stdio::null(),
    }
}

/// Runs `cmd`, a local process that carries out `job` (on this machine, or
/// through whatever reaches the host), with the job's streams. The one place a
/// job becomes a process: the hosts that are local processes build the command
/// and hand it here.
fn run_process(mut cmd: Command, job: &mut Job) -> std::io::Result<Ran> {
    cmd.stdin(match job.stdin {
        Input::Null => Stdio::null(),
        Input::Inherit => Stdio::inherit(),
        Input::Feed(_) => Stdio::piped(),
    })
    .stdout(stdio(job.stdout))
    .stderr(stdio(job.stderr));
    crate::output::trace(&cmd);
    let mut child = cmd.spawn()?;
    let fed = match &mut job.stdin {
        Input::Feed(feed) => {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("the command's stdin was not piped"))?;
            // Dropping the pipe at the end of the block is what tells the
            // command its input is over.
            feed(&mut stdin)
        }
        _ => Ok(()),
    };
    let out = child.wait_with_output()?;
    Ok(Ran {
        status: out.status,
        stdout: out.stdout,
        stderr: out.stderr,
        fed,
    })
}

/// Starts `cmd` as a process that keeps running (a tunnel): no input, no output,
/// and its errors where `chatter` says.
fn start_process(mut cmd: Command, chatter: Stream) -> std::io::Result<Box<dyn Child>> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stdio(chatter));
    crate::output::trace(&cmd);
    Ok(Box::new(LocalChild(cmd.spawn()?)))
}

/// Runs `cmd` with a terminal, for a person to work in.
fn run_interactive(mut cmd: Command) -> std::io::Result<ExitStatus> {
    crate::output::trace(&cmd);
    cmd.status()
}

/// How often and how fast a host retries a step after losing its connection.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Tries in all, the first included; 1 means never retry.
    pub tries: u32,
    /// The wait before the second try; it doubles after each.
    pub first_delay: Duration,
}

impl RetryPolicy {
    /// For a host whose connection cannot be lost.
    pub const NEVER: Self = Self {
        tries: 1,
        first_delay: Duration::ZERO,
    };
}

/// A file made by [`Host::write_scratch`], removed when dropped.
pub struct ScratchFile<'a> {
    path: String,
    cleanup: Option<Box<dyn FnOnce() + 'a>>,
}

impl<'a> ScratchFile<'a> {
    /// A file that is removed by `cleanup` once this is dropped.
    pub fn new(path: String, cleanup: impl FnOnce() + 'a) -> Self {
        Self {
            path,
            cleanup: Some(Box::new(cleanup)),
        }
    }

    /// Its path on the host.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for ScratchFile<'_> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

/// What [`Host::read_first`] found.
#[derive(Debug, PartialEq, Eq)]
pub struct FirstFile {
    /// Every match, in order.
    pub paths: Vec<PathBuf>,
    /// The contents of the first match, if there is one.
    pub contents: Option<Vec<u8>>,
}

/// A machine lodge can run commands on.
///
/// Implement the required methods (and the connection ones, if the link can
/// drop); the file and script helpers below then work unchanged.
pub trait Host: Send + Sync {
    /// How a person refers to this host, or `None` for the machine lodge runs
    /// on: the tail of a message about where something was or was not found.
    fn label(&self) -> Option<String>;

    /// The names this host answers to when a secret's `targets` is matched:
    /// `local` for this machine, otherwise the destination and its host part.
    fn names(&self) -> Vec<String>;

    /// Whether lodge's own working directory means something on this host, so a
    /// project path may be left out.
    fn shares_cwd(&self) -> bool;

    /// Runs `job` on the host once and waits for it. Errors only when it could
    /// not be run at all; a non-zero exit is in the [`Ran`]. Use
    /// [`exec`](Self::exec) for a step that is retried after a lost connection.
    fn run(&self, job: &mut Job) -> std::io::Result<Ran>;

    /// Runs `argv` on the host with a terminal, for a person to work in, and
    /// waits for it.
    fn interactive(&self, argv: &[&str]) -> std::io::Result<ExitStatus>;

    /// Starts a process that tunnels local port `local` to `to` (`ip:port`, as
    /// seen from the host) until killed, with its errors going where `chatter`
    /// says; or `None` when what the host can reach is reachable from here too,
    /// so the caller proxies in-process. It cannot prompt, so it relies on what
    /// [`connect`](Self::connect) set up.
    fn tunnel(
        &self,
        _local: u16,
        _to: &str,
        _chatter: Stream,
    ) -> Option<std::io::Result<Box<dyn Child>>> {
        None
    }

    /// Establishes the connection while the terminal is ours, so a password or a
    /// second factor is asked for once, here. Nothing to do for a host that is
    /// always there.
    fn connect(&self) -> Result<()> {
        Ok(())
    }

    /// Whether the host refuses a terminal, which makes
    /// [`interactive`](Self::interactive) fail at once. Best effort.
    fn pty_denied(&self) -> bool {
        false
    }

    /// Whether a step that ended like this lost its connection to the host
    /// (rather than failing on the host), so it may be tried again.
    fn link_lost(&self, _status: ExitStatus, _stderr: &[u8]) -> bool {
        false
    }

    /// Discards a connection that is dead, so the next step opens a new one.
    fn reset_link(&self) {}

    /// How a lost connection is retried.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::NEVER
    }

    /// `" on <host>"` for another machine, nothing for this one.
    fn on_host(&self) -> String {
        self.label().map(|h| format!(" on {h}")).unwrap_or_default()
    }

    /// Runs `job`, rerunning it after a lost connection if it is
    /// [`Repeat::Safe`] (see [`attempt`]).
    fn exec(&self, mut job: Job) -> std::io::Result<Ran> {
        let repeat = job.repeat;
        attempt(self, repeat, || self.run(&mut job))
    }

    /// Runs `sh -c <script>` on the host and captures its output. A lost
    /// connection reruns the script, so it must be safe to run twice: a read or
    /// a probe.
    fn sh_output(&self, script: &str) -> Result<Ran> {
        let out = self
            .exec(Job::shell(script).capture().repeat(Repeat::Safe))
            .context("running a command on the host")?;
        // With a verbose transport the debug chatter lands on stderr; surface it.
        if crate::output::is_verbose() && !out.stderr.is_empty() {
            crate::output::raw_stderr(&out.stderr_text());
        }
        Ok(out)
    }

    /// Runs `cmd` with `dir` as its working directory on the host, with
    /// inherited stdio.
    fn run_in(&self, dir: &Path, cmd: &CmdLine) -> Result<ExitStatus> {
        let job = Job::new(Line::from(cmd))
            .in_dir(Some(dir))
            .stdin(Input::Inherit);
        Ok(self.exec(job).context("spawning host command")?.status)
    }

    /// Writes `content` to a new scratch file on the host, private to the user
    /// and unique whatever else is in its temporary folder, which is removed
    /// when the returned guard is dropped. `_name` says what it is for, for
    /// hosts that keep the file somewhere a person can see it.
    fn write_scratch(&self, _name: &str, content: &str) -> Result<ScratchFile<'_>> {
        let mut feed = |stdin: &mut dyn Write| stdin.write_all(content.as_bytes());
        let job = Job::shell(WRITE_SCRATCH_SCRIPT)
            .stdin(Input::Feed(&mut feed))
            .stdout(Stream::Capture);
        let out = self.exec(job).context("running a command on the host")?;
        let path = out.stdout_text();
        out.fed.context("writing to the command's stdin")?;
        if !out.status.success() || path.is_empty() {
            anyhow::bail!("writing a scratch file{} failed", self.on_host());
        }
        Ok(ScratchFile::new(path.clone(), move || {
            self.remove_file(&path)
        }))
    }

    /// Removes `path` on the host. Best effort: a leftover scratch file is not
    /// worth failing over.
    fn remove_file(&self, path: &str) {
        let script = format!("rm -f {}", shell_quote(path));
        if let Err(e) = self.sh_output(&script) {
            crate::output::warning!("could not remove {path}{}: {e:#}", self.on_host());
        }
    }

    /// The contents of the file `path` on the host, or `None` if there is no
    /// such file.
    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let q = shell_quote(&path.to_string_lossy());
        // Exit 3 means "no such file", told apart from the transport's failures.
        let out = self.sh_output(&format!("[ -f {q} ] || exit 3; cat {q}"))?;
        match out.status.code() {
            Some(0) => Ok(Some(out.stdout)),
            Some(3) => Ok(None),
            _ => anyhow::bail!(
                "reading {}{} failed: {}",
                path.display(),
                self.on_host(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// The folder `dir` on the host as an absolute path with every symlink, `.`
    /// and `..` resolved, so one folder has one name however it was spelled. An
    /// error if it doesn't exist. A relative `dir` is relative to the host's
    /// working or login directory.
    fn canonical_dir(&self, dir: &Path) -> Result<PathBuf> {
        let out = self.sh_output(&canonical_dir_script(dir))?;
        let path = String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string();
        if !out.status.success() || !path.starts_with('/') {
            anyhow::bail!(
                "project folder {} does not exist{}: {}",
                dir.display(),
                self.on_host(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(PathBuf::from(path))
    }

    /// The regular files on the host that match `patterns`, in the order of the
    /// patterns (and sorted within one), and the contents of the first of them.
    /// A path component that is exactly `*` matches any one name; nothing else
    /// is special. One round trip on another machine.
    fn read_first(&self, patterns: &[PathBuf]) -> Result<FirstFile> {
        let out = self.sh_output(&read_first_script(patterns))?;
        if !out.status.success() {
            anyhow::bail!(
                "listing files{} failed ({}): {}",
                self.on_host(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_read_first(&out.stdout)
            .with_context(|| format!("unexpected answer listing files{}", self.on_host()))
    }

    /// Makes the directory `dir` on the host readable here: its contents are
    /// copied into `dest` (one `tar` stream, unpacked in-process) and `dest` is
    /// returned. A host that shares this filesystem returns `dir` itself.
    fn fetch_dir(&self, dir: &Path, dest: &Path) -> Result<PathBuf> {
        let script = format!("tar -C {} -cf - .", shell_quote(&dir.to_string_lossy()));
        let out = self.sh_output(&script)?;
        if !out.status.success() {
            anyhow::bail!(
                "reading {}{} failed: {}",
                dir.display(),
                self.on_host(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        std::fs::create_dir_all(dest)?;
        tar::Archive::new(out.stdout.as_slice())
            .unpack(dest)
            .with_context(|| format!("unpacking {}{}", dir.display(), self.on_host()))?;
        Ok(dest.to_path_buf())
    }

    /// The uid and gid of the user lodge runs as on the host (that is who owns a
    /// bind-mounted workspace).
    fn uid_gid(&self) -> Result<(u32, u32)> {
        let out = self.sh_output("id -u; id -g")?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut ids = stdout.lines().map(|l| l.trim().parse::<u32>());
        match (ids.next(), ids.next()) {
            (Some(Ok(uid)), Some(Ok(gid))) => Ok((uid, gid)),
            _ => anyhow::bail!("could not read the host user's uid/gid (got {stdout:?})"),
        }
    }
}

impl dyn Host + '_ {
    /// [`attempt`] for a host behind a pointer.
    pub fn attempt<T: Outcome>(
        &self,
        repeat: Repeat,
        op: impl FnMut() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        attempt(self, repeat, op)
    }
}

/// Runs one step against `host`. `op` is called afresh each time, so it builds
/// its command (and any stdin) anew. If the host reports the connection lost
/// and the step is [`Repeat::Safe`], the dead connection is dropped and the step
/// runs again, waiting 1 s, then 2 s and so on, up to the host's try limit. A
/// step that is not safe to repeat is run once and the loss reported.
pub fn attempt<H: Host + ?Sized, T: Outcome>(
    host: &H,
    repeat: Repeat,
    op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    retry(
        &host.retry_policy(),
        || host.label().unwrap_or_default(),
        repeat,
        |out: &T| host.link_lost(out.status(), out.stderr()),
        || host.reset_link(),
        op,
    )
}

fn retry<T>(
    policy: &RetryPolicy,
    name: impl Fn() -> String,
    repeat: Repeat,
    lost: impl Fn(&T) -> bool,
    mut reset: impl FnMut(),
    mut op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut delay = policy.first_delay;
    let mut attempt = 1;
    loop {
        let out = op()?;
        if !lost(&out) {
            return Ok(out);
        }
        if repeat == Repeat::Never {
            crate::output::warning!(
                "lost the connection to {} during a step that is not safe to repeat; \
                 run the command again",
                name()
            );
            return Ok(out);
        }
        if attempt >= policy.tries {
            return Ok(out);
        }
        attempt += 1;
        crate::output::warning!(
            "lost the connection to {}; retrying ({attempt}/{})",
            name(),
            policy.tries
        );
        std::thread::sleep(delay);
        delay *= 2;
        reset();
    }
}

/// The script behind [`Host::write_scratch`]: a new owner-only file from
/// `mktemp`, filled from stdin, its path printed. Removed again if filling it
/// fails.
pub(crate) const WRITE_SCRATCH_SCRIPT: &str = "f=$(mktemp \"${TMPDIR:-/tmp}/lodge-XXXXXXXXXX\") \
     && { cat > \"$f\" || { rm -f \"$f\"; exit 1; }; } && printf '%s' \"$f\"";

/// The `sh` script behind [`Host::canonical_dir`].
fn canonical_dir_script(dir: &Path) -> String {
    format!("cd -- {} && pwd -P", shell_quote(&dir.to_string_lossy()))
}

/// The `sh` script behind [`Host::read_first`]: each match ends with a NUL, an
/// extra NUL ends the list (paths are never empty), then the first match's bytes
/// follow. Literal path components are quoted; a `*` component is left bare for
/// the shell to expand.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    /// Runs a step that answers with `codes` in turn, on a host that calls 255 a
    /// lost connection; returns the last code, how often the step ran and how
    /// often the connection was reset.
    fn run(repeat: Repeat, codes: &[i32]) -> (i32, usize, usize) {
        let (mut ran, mut resets) = (0, 0);
        let policy = RetryPolicy {
            tries: 3,
            first_delay: Duration::ZERO,
        };
        let status = retry(
            &policy,
            || "h".into(),
            repeat,
            |s: &ExitStatus| s.code() == Some(255),
            || resets += 1,
            || {
                ran += 1;
                Ok(exit(codes[(ran - 1).min(codes.len() - 1)]))
            },
        )
        .unwrap();
        (status.code().unwrap(), ran, resets)
    }

    #[test]
    fn a_repeatable_step_is_rerun_after_the_connection_drops() {
        assert_eq!(run(Repeat::Safe, &[255, 255, 0]), (0, 3, 2));
    }

    #[test]
    fn a_step_that_works_is_run_once_and_the_connection_left_alone() {
        assert_eq!(run(Repeat::Safe, &[0]), (0, 1, 0));
        assert_eq!(
            run(Repeat::Safe, &[1]),
            (1, 1, 0),
            "the command's own failure"
        );
    }

    #[test]
    fn retrying_gives_up_after_three_tries() {
        assert_eq!(run(Repeat::Safe, &[255]), (255, 3, 2));
    }

    #[test]
    fn a_step_that_is_not_safe_to_repeat_runs_once_even_when_the_connection_drops() {
        assert_eq!(run(Repeat::Never, &[255, 0]), (255, 1, 0));
    }

    #[test]
    fn a_host_that_cannot_lose_its_connection_never_retries() {
        let mut ran = 0;
        let status = attempt(&LocalHost, Repeat::Safe, || {
            ran += 1;
            Ok(exit(255))
        })
        .unwrap();
        assert_eq!((status.code(), ran), (Some(255), 1));
    }

    #[test]
    fn the_script_finds_and_reads_like_the_local_lookup() {
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
            let out = Command::new("sh")
                .args(["-c", &read_first_script(patterns)])
                .output()
                .unwrap();
            assert!(out.status.success());
            parse_read_first(&out.stdout).unwrap()
        };
        let patterns = [p.join("missing"), p.join("sub/*/x.json"), p.join("a.json")];
        assert_eq!(run(&patterns), LocalHost.read_first(&patterns).unwrap());
        assert_eq!(run(&patterns).contents.as_deref(), Some(&b"AA"[..]));
        let none = [p.join("nope"), p.join("*/nope")];
        assert_eq!(run(&none), LocalHost.read_first(&none).unwrap());
        assert!(run(&none).paths.is_empty());
    }

    #[test]
    fn the_canonical_dir_script_resolves_like_the_local_lookup() {
        let tmp = crate::tmpfile::TempDir::new("canon").unwrap();
        std::fs::create_dir_all(tmp.path().join("a b/c")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a b"), tmp.path().join("link")).unwrap();
        let spelled = tmp.path().join("link/c/../c/.");
        let out = Command::new("sh")
            .args(["-c", &canonical_dir_script(&spelled)])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end()),
            LocalHost.canonical_dir(&spelled).unwrap()
        );
        assert!(
            LocalHost
                .canonical_dir(&tmp.path().join("missing"))
                .is_err()
        );
    }

    #[test]
    fn the_scratch_script_makes_a_private_file_and_prints_it() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::tmpfile::TempDir::new("scratch").unwrap();
        let mut child = Command::new("sh")
            .args(["-c", WRITE_SCRATCH_SCRIPT])
            .env("TMPDIR", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
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

    #[test]
    fn the_provided_helpers_work_through_the_run_primitive_alone() {
        // A host that only implements `run` (locally, with `sh`) still reads,
        // resolves and lists, and cleans its scratch file up: what a new
        // implementation gets without writing any of it.
        struct Bare;
        impl Host for Bare {
            fn label(&self) -> Option<String> {
                Some("bare".into())
            }
            fn names(&self) -> Vec<String> {
                vec!["bare".into()]
            }
            fn shares_cwd(&self) -> bool {
                false
            }
            fn run(&self, job: &mut Job) -> std::io::Result<Ran> {
                LocalHost.run(job)
            }
            fn interactive(&self, argv: &[&str]) -> std::io::Result<ExitStatus> {
                LocalHost.interactive(argv)
            }
        }
        let tmp = crate::tmpfile::TempDir::new("bare").unwrap();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        let host: &dyn Host = &Bare;
        assert_eq!(
            host.read_file(&tmp.path().join("f")).unwrap().as_deref(),
            Some(&b"x"[..])
        );
        assert_eq!(host.read_file(&tmp.path().join("g")).unwrap(), None);
        assert_eq!(host.on_host(), " on bare");
        let scratch = host.write_scratch("t", "hello").unwrap();
        let path = PathBuf::from(scratch.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        drop(scratch);
        assert!(!path.exists());
        assert_eq!(host.uid_gid().unwrap().0, LocalHost.uid_gid().unwrap().0);
    }
}
