//! A machine reached over the system `ssh`: [`SshHost`] is the [`Host`] for it.
//!
//! This module holds everything about `ssh`: parsing a `--remote` destination,
//! connection multiplexing, and the few ssh invocations lodge needs. Every ssh
//! command is built from [`base`] here, so port, multiplexing and `-v` are
//! applied once. Nothing outside this module names `ssh`.

use anyhow::Context as _;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use super::{
    Child, Host, Job, Line, Ran, Repeat, RetryPolicy, Stream, run_interactive, run_process,
    start_process,
};
use crate::shell::shell_quote;

/// A host reached with `ssh <destination>`: a `~/.ssh/config` alias, `user@host`,
/// `user@host:port`, `[v6]:port`, with or without an `ssh://` prefix. Proxies,
/// jump hosts and agents are the user's ssh configuration's business.
pub struct SshHost {
    remote: String,
    /// Whether a command may ask for a password or a second factor on the
    /// terminal. Off for a process that has none (the forward daemon): a prompt
    /// there would hang, or stop the process, instead of failing.
    prompts: bool,
}

impl SshHost {
    /// A host whose commands may prompt on the terminal.
    pub fn new(remote: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            prompts: true,
        }
    }

    /// A host for a process with no terminal: every command it runs fails when
    /// it would need a login, and rides on the connection [`connect`] opened
    /// otherwise.
    pub fn unattended(remote: impl Into<String>) -> Self {
        Self {
            prompts: false,
            ..Self::new(remote)
        }
    }
}

/// Splits a `--remote` destination into the ssh destination and an optional
/// port. Accepted forms: a `~/.ssh/config` Host alias, `user@host`,
/// `user@host:port`, `[v6addr]:port`, and any of those with an `ssh://`
/// prefix. A bare IPv6 address (more than one `:` and no brackets) is left
/// untouched rather than misread as host:port.
fn split(remote: &str) -> (String, Option<u16>) {
    let remote = remote.strip_prefix("ssh://").unwrap_or(remote);
    let host_start = remote.rfind('@').map(|i| i + 1).unwrap_or(0);
    let (user, host) = remote.split_at(host_start);

    // Bracketed IPv6: [addr] or [addr]:port. ssh wants the bare address.
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let addr = &rest[..end];
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse().ok());
            return (format!("{user}{addr}"), port);
        }
        return (remote.to_string(), None);
    }

    if let Some((h, p)) = host.rsplit_once(':')
        && !h.is_empty()
        && !h.contains(':')
        && let Ok(port) = p.parse::<u16>()
    {
        return (format!("{user}{h}"), Some(port));
    }
    (remote.to_string(), None)
}

/// Socket used to multiplex every ssh invocation against the same `--remote`
/// destination over one already-authenticated connection (`ControlMaster`),
/// keyed by the raw `remote` string so distinct hosts/ports get distinct
/// sockets. A `ldg up` issues dozens of separate `ssh` calls (one per
/// `docker exec`/`cp`); without multiplexing each renegotiates its own TCP +
/// auth handshake, which is slow and, on a flaky or non-default-port hop,
/// occasionally just fails — silently degrading some operations (e.g.
/// chown'ing injected config) rather than the whole command.
fn control_path(remote: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    remote.hash(&mut hasher);
    let dir = std::env::temp_dir().join("lodge-ssh");
    // If the directory can't be made, ssh can't bind its control socket and falls
    // back to a direct connection, so this failure only costs connection reuse.
    #[allow(clippy::let_underscore_must_use, reason = "see above")]
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    dir.join(format!("{:x}.sock", hasher.finish()))
}

/// The `ssh` invocation for `remote` with the port (from a `host:port`
/// destination), connection multiplexing and `--verbose` flags applied. Returns
/// the destination separately: ssh options must precede it, so a caller adds
/// its own options, then the destination, then the remote command.
///
/// Private on purpose: every `ssh` command lodge runs is built in this module.
fn base(remote: &str) -> (Command, String) {
    let (dest, port) = split(remote);
    let mut c = Command::new("ssh");
    if let Some(p) = port {
        c.args(["-p", &p.to_string()]);
    }
    c.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=10m"]);
    // A link that goes quiet is declared dead after about 45 s, so a step fails
    // (and can be retried) instead of hanging; see [`retry`].
    c.args([
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    c.arg("-o")
        .arg(format!("ControlPath={}", control_path(remote).display()));
    let v = crate::output::verbosity().min(3);
    if v > 0 {
        c.arg(format!("-{}", "v".repeat(v as usize)));
    }
    (c, dest)
}

/// A command that runs `remote_command` (a string for the remote login shell)
/// on `remote`, never prompting if `prompts` is false. The caller sets stdio and
/// runs it.
fn command(remote: &str, remote_command: &str, prompts: bool) -> Command {
    let (mut c, dest) = base(remote);
    if !prompts {
        c.args(["-o", "BatchMode=yes"]);
    }
    c.arg(dest).arg(remote_command);
    c
}

/// A tunnel from local port `local` to `to` ("ip:port", as reachable from the
/// remote host). Runs until killed; the caller sets stdio and spawns it.
///
/// It is started by the forward daemon, which cannot ask for a password, so it
/// never prompts (`BatchMode`) and relies on the connection [`connect`] opened,
/// or on key or agent auth, to get through.
fn tunnel(remote: &str, local: u16, to: &str) -> Command {
    let (mut c, dest) = base(remote);
    c.args([
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-L",
        &format!("{local}:{to}"),
    ])
    .arg(dest);
    c
}

/// A command that keeps `remote_command` running on `remote` until killed. A
/// terminal is forced (`-tt`) so that killing this ssh hangs up the remote
/// command too, instead of leaving it behind. It never prompts (`BatchMode`),
/// like [`tunnel`].
fn keep_running(remote: &str, remote_command: &str) -> Command {
    let (mut c, dest) = base(remote);
    c.args(["-tt", "-o", "BatchMode=yes"])
        .arg(dest)
        .arg(remote_command);
    c
}

/// How often and how fast a lost connection is retried: a link that goes quiet
/// is declared dead by `ServerAlive*` after about 45 s, then a step is tried up
/// to three times, waiting 1 s, then 2 s.
const POLICY: RetryPolicy = RetryPolicy {
    tries: 3,
    first_delay: Duration::from_secs(1),
};

/// Whether a finished step lost the connection (ssh exits 255) rather than the
/// remote command failing. A failure that trying again cannot change (bad
/// credentials, an unknown or changed host key, a configuration ssh rejects) is
/// not a lost connection.
fn connection_lost(status: ExitStatus, stderr: &[u8]) -> bool {
    const PERMANENT: [&str; 4] = [
        "Permission denied",
        "Host key verification failed",
        "ControlPath too long",
        "Bad configuration option",
    ];
    let err = String::from_utf8_lossy(stderr);
    status.code() == Some(255) && !PERMANENT.iter().any(|m| err.contains(m))
}

/// Asks the master connection to `remote` to exit, so the next ssh opens a new
/// one even if the old one is dead but still holds its socket. Quiet; fails
/// harmlessly when there is no master.
fn drop_master(remote: &str) {
    let (mut c, dest) = base(remote);
    c.args(["-O", "exit"])
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[allow(
        clippy::let_underscore_must_use,
        reason = "there may be no master to stop"
    )]
    let _ = c.status();
}

/// Opens the shared connection to `remote` before anything else uses it, so a
/// password, a passphrase or a 2FA prompt is answered once, here, with the
/// terminal available, and every later ssh (and the forward daemon's tunnels)
/// rides on it. Runs a no-op remote command and leaves the master in the
/// background (`ControlPersist`); a connection that is down is retried.
fn connect(remote: &str) -> anyhow::Result<()> {
    let out = crate::host::attempt(&SshHost::new(remote), Repeat::Safe, || {
        let (mut c, dest) = base(remote);
        // Prompts are read from the terminal, not stdin, so ssh's stderr can be
        // captured to be reported (or judged) afterwards.
        c.arg(dest)
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        crate::output::trace(&c);
        c.output()
    })
    .context("spawning ssh")?;
    if !out.status.success() {
        anyhow::bail!(
            "cannot connect to {remote}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a connection attempt that could not prompt ended because the server
/// wants a login (a password, a second factor, a key that is not loaded) rather
/// than because it could not be reached. Only a person at a terminal can answer
/// that, and doing so opens the shared connection for everyone else.
fn login_wanted(status: ExitStatus, stderr: &[u8]) -> bool {
    !status.success() && String::from_utf8_lossy(stderr).contains("Permission denied")
}

/// Whether `remote` answers but would need a login to let us in: a connection
/// that cannot prompt (`BatchMode`) is tried, and fails with the server's
/// refusal. Succeeds (so `false`) while the shared connection is up or a key
/// gets through. Best-effort and quiet: a spawn error or a timeout yields `false`.
fn awaiting_login(remote: &str) -> bool {
    let (mut c, dest) = base(remote);
    c.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .arg(dest)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c.output()
        .map(|out| login_wanted(out.status, &out.stderr))
        .unwrap_or(false)
}

/// Whether the remote SSH server denies PTY allocation, which makes an
/// interactive `docker exec -it` shell impossible. Forces a PTY (`-tt`) and runs
/// `tty`: a server that refuses it makes ssh fail with "Pseudo-terminal
/// allocation request failed" (or, if it proceeds without one, `tty` prints "not
/// a tty"). Best-effort and quiet: any spawn error just yields `false`.
fn pty_denied(remote: &str) -> bool {
    let (mut cmd, dest) = base(remote);
    cmd.arg("-tt")
        .arg(dest)
        .arg("tty")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(out) = cmd.output() else {
        return false;
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    combined.contains("allocation request failed") || combined.contains("not a tty")
}

/// A local command that opens an interactive session on `remote` running
/// `remote_command`, with a PTY forced (`-tt`).
fn interactive(remote: &str, remote_command: &str) -> Command {
    let (mut cmd, dest) = base(remote);
    cmd.arg("-tt").arg(dest).arg(remote_command);
    cmd
}

impl SshHost {
    /// `ssh <host> <line>`, the line run by the remote login shell. A script is
    /// forced to `sh` semantics rather than the remote user's own shell, and a
    /// working directory becomes a `cd` in front.
    fn command(&self, line: &Line, dir: Option<&Path>) -> Command {
        let body = match line {
            Line::Shell(script) => format!("sh -c {}", shell_quote(script)),
            Line::Argv(argv) => argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        };
        let script = match dir {
            Some(dir) => format!("cd {} && {body}", shell_quote(&dir.to_string_lossy())),
            None => body,
        };
        command(&self.remote, &script, self.prompts)
    }
}

impl SshHost {
    /// The local `ssh` that opens `argv` on the host with a terminal.
    fn interactive_command(&self, argv: &[&str]) -> Command {
        let line = argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        interactive(&self.remote, &line)
    }
}

impl Host for SshHost {
    fn label(&self) -> Option<String> {
        Some(self.remote.clone())
    }

    fn names(&self) -> Vec<String> {
        let mut names = vec![self.remote.clone()];
        let (host, _port) = split(&self.remote);
        for n in [
            host.clone(),
            host.rsplit('@').next().unwrap_or("").to_string(),
        ] {
            if !n.is_empty() && !names.contains(&n) {
                names.push(n);
            }
        }
        names
    }

    fn shares_cwd(&self) -> bool {
        false
    }

    fn run(&self, job: &mut Job) -> std::io::Result<Ran> {
        run_process(self.command(&job.line, job.dir), job)
    }

    fn interactive(&self, argv: &[&str]) -> std::io::Result<ExitStatus> {
        run_interactive(self.interactive_command(argv))
    }

    fn tunnel(
        &self,
        local: u16,
        to: &str,
        chatter: Stream,
    ) -> Option<std::io::Result<Box<dyn Child>>> {
        Some(start_process(tunnel(&self.remote, local, to), chatter))
    }

    fn spawn(&self, argv: &[&str], chatter: Stream) -> std::io::Result<Box<dyn Child>> {
        let line = argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        start_process(keep_running(&self.remote, &line), chatter)
    }

    fn connect(&self) -> anyhow::Result<()> {
        connect(&self.remote)
    }

    fn awaiting_login(&self) -> bool {
        awaiting_login(&self.remote)
    }

    fn pty_denied(&self) -> bool {
        pty_denied(&self.remote)
    }

    fn link_lost(&self, status: ExitStatus, stderr: &[u8]) -> bool {
        connection_lost(status, stderr)
    }

    fn reset_link(&self) {
        drop_master(&self.remote);
    }

    fn retry_policy(&self) -> RetryPolicy {
        POLICY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn plain_alias_and_user_host_pass_through() {
        assert_eq!(split("myhost"), ("myhost".into(), None));
        assert_eq!(split("user@host"), ("user@host".into(), None));
    }

    #[test]
    fn trailing_port_is_extracted() {
        assert_eq!(split("host:2222"), ("host".into(), Some(2222)));
        assert_eq!(split("user@host:2222"), ("user@host".into(), Some(2222)));
    }

    #[test]
    fn ssh_url_prefix_is_stripped() {
        assert_eq!(
            split("ssh://user@host:2222"),
            ("user@host".into(), Some(2222))
        );
        assert_eq!(split("ssh://host"), ("host".into(), None));
    }

    #[test]
    fn bare_ipv6_is_not_misread_as_port() {
        assert_eq!(split("fe80::1"), ("fe80::1".into(), None));
        assert_eq!(split("user@fe80::1"), ("user@fe80::1".into(), None));
    }

    #[test]
    fn bracketed_ipv6_with_port() {
        assert_eq!(split("[fe80::1]:2222"), ("fe80::1".into(), Some(2222)));
        assert_eq!(
            split("user@[fe80::1]:2222"),
            ("user@fe80::1".into(), Some(2222))
        );
        assert_eq!(split("[fe80::1]"), ("fe80::1".into(), None));
    }

    #[test]
    fn non_numeric_suffix_is_not_a_port() {
        assert_eq!(split("host:notaport"), ("host:notaport".into(), None));
    }

    fn args(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_remote_command_ends_with_the_destination_and_the_script() {
        let a = args(&command("user@host:2222", "docker ps", true));
        assert_eq!(&a[..2], ["-p", "2222"]);
        assert_eq!(&a[a.len() - 2..], ["user@host", "docker ps"]);
    }

    #[test]
    fn an_unattended_host_never_prompts_and_an_ordinary_one_may() {
        let line = Line::Shell("true".into());
        let quiet = args(&SshHost::unattended("h").command(&line, None));
        assert!(quiet.contains(&"BatchMode=yes".to_string()), "{quiet:?}");
        let a = args(&SshHost::new("h").command(&line, None));
        assert!(!a.contains(&"BatchMode=yes".to_string()), "{a:?}");
    }

    #[test]
    fn a_tunnel_forwards_the_local_port_to_the_target() {
        let a = args(&tunnel("host", 8080, "172.18.0.5:80"));
        let l = a.iter().position(|x| x == "-L").unwrap();
        assert_eq!(a[l + 1], "8080:172.18.0.5:80");
        assert_eq!(a.last().unwrap(), "host", "the destination comes last");
        assert!(a.contains(&"ExitOnForwardFailure=yes".to_string()));
    }

    #[test]
    fn a_process_kept_running_has_a_terminal_so_it_hangs_up_with_the_connection_and_never_prompts()
    {
        let a = args(&keep_running("user@host", "'kubectl' 'port-forward' 'x'"));
        assert!(a.contains(&"-tt".to_string()), "{a:?}");
        assert!(a.contains(&"ControlMaster=auto".to_string()), "{a:?}");
        assert_eq!(
            &a[a.len() - 3..],
            ["BatchMode=yes", "user@host", "'kubectl' 'port-forward' 'x'"]
        );
    }

    #[test]
    fn an_interactive_session_forces_a_pty_reuses_the_connection_and_quotes_the_command() {
        let c = SshHost::new("user@host:2222")
            .interactive_command(&["docker", "exec", "-it", "c", "a b"]);
        let a = args(&c);
        assert_eq!(&a[..2], ["-p", "2222"]);
        assert!(a.contains(&"ControlMaster=auto".to_string()), "{a:?}");
        assert_eq!(
            &a[a.len() - 3..],
            ["-tt", "user@host", "'docker' 'exec' '-it' 'c' 'a b'"]
        );
    }

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn only_ssh_exiting_255_is_a_lost_connection() {
        assert!(connection_lost(exit(255), b""));
        assert!(!connection_lost(exit(0), b""));
        assert!(!connection_lost(exit(1), b""));
        assert!(
            !connection_lost(ExitStatus::from_raw(9), b""),
            "killed by a signal"
        );
    }

    #[test]
    fn only_a_server_that_wants_a_login_is_awaiting_one() {
        let wants = |code, err: &str| login_wanted(exit(code), err.as_bytes());
        assert!(wants(255, "u@h: Permission denied (publickey,password)."));
        assert!(!wants(0, ""), "connected");
        assert!(!wants(
            255,
            "ssh: connect to host h port 22: Connection refused"
        ));
        assert!(!wants(255, "Host key verification failed."));
    }

    #[test]
    fn a_refused_login_is_not_a_lost_connection() {
        let lost = |code, err: &str| connection_lost(exit(code), err.as_bytes());
        assert!(lost(
            255,
            "ssh: connect to host h port 22: Connection refused"
        ));
        assert!(!lost(255, "u@h: Permission denied (publickey)."));
        assert!(!lost(255, "Host key verification failed."));
        assert!(!lost(255, "ControlPath too long ('/x' >= 108 bytes)"));
        assert!(!lost(1, "Connection refused"));
    }

    #[test]
    fn a_host_runs_commands_through_ssh_with_sh_semantics_and_a_directory() {
        let host = SshHost::new("user@host:2222");
        let a = args(&host.command(&Line::Shell("echo hi".into()), None));
        assert_eq!(&a[..2], ["-p", "2222"]);
        assert_eq!(&a[a.len() - 2..], ["user@host", "sh -c 'echo hi'"]);
        let argv = Line::Argv(vec!["docker".to_string(), "ps -a".to_string()]);
        let a = args(&host.command(&argv, Some(Path::new("/p q"))));
        assert_eq!(a.last().unwrap(), "cd '/p q' && 'docker' 'ps -a'");
    }

    #[test]
    fn the_host_names_cover_the_alias_the_user_and_the_machine() {
        assert_eq!(SshHost::new("build").names(), ["build"]);
        assert_eq!(
            SshHost::new("me@build.example:2222").names(),
            ["me@build.example:2222", "me@build.example", "build.example"]
        );
    }
}
