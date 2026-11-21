//! Everything about `ssh`: parsing a `--remote` destination, connection
//! multiplexing, and the few ssh invocations lodge needs. Every ssh command is
//! built from [`base`] here, so port, multiplexing and `-v` are applied once.

use anyhow::Context as _;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::Duration;

/// Splits a `--remote` destination into the ssh destination and an optional
/// port. Accepted forms: a `~/.ssh/config` Host alias, `user@host`,
/// `user@host:port`, `[v6addr]:port`, and any of those with an `ssh://`
/// prefix. A bare IPv6 address (more than one `:` and no brackets) is left
/// untouched rather than misread as host:port.
pub fn split(remote: &str) -> (String, Option<u16>) {
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
/// on `remote`. The caller sets stdio and runs it.
pub fn command(remote: &str, remote_command: &str) -> Command {
    let (mut c, dest) = base(remote);
    c.arg(dest).arg(remote_command);
    c
}

/// Runs `sh -c <script>` on the remote and returns its output; the command line
/// is traced at `-v`. The script is rerun if the connection drops, so it must be
/// safe to run twice (a read, a probe).
pub fn sh_output(host: &str, script: &str) -> anyhow::Result<std::process::Output> {
    let out = retry(host, Repeat::Safe, || {
        let mut cmd = command(
            host,
            &format!("sh -c {}", crate::shell::shell_quote(script)),
        );
        crate::output::trace(&cmd);
        cmd.output()
    })
    .context("spawning ssh")?;
    // With ssh -v the debug chatter lands on stderr; surface it.
    if crate::output::is_verbose() && !out.stderr.is_empty() {
        crate::output::raw_stderr(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(out)
}

/// The remote script behind [`write_scratch`]: a new owner-only file from
/// `mktemp`, filled from stdin, its path printed. Removed again if filling it
/// fails.
pub(crate) const WRITE_SCRATCH_SCRIPT: &str = "f=$(mktemp \"${TMPDIR:-/tmp}/lodge-XXXXXXXXXX\") \
     && { cat > \"$f\" || { rm -f \"$f\"; exit 1; }; } && printf '%s' \"$f\"";

/// Writes `content` to a new scratch file on the remote host, made with
/// `mktemp` (unique and owner-only, whatever else is in the remote `/tmp`), and
/// returns its path.
pub fn write_scratch(host: &str, content: &str) -> anyhow::Result<String> {
    use std::io::Write as _;
    let mut cmd = command(
        host,
        &format!("sh -c {}", crate::shell::shell_quote(WRITE_SCRATCH_SCRIPT)),
    );
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    crate::output::trace(&cmd);
    let mut child = cmd.spawn().context("spawning ssh")?;
    child
        .stdin
        .take()
        .context("ssh stdin was not piped")?
        .write_all(content.as_bytes())
        .context("writing to ssh stdin")?;
    let out = child.wait_with_output().context("waiting for ssh")?;
    let path = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() || path.is_empty() {
        anyhow::bail!("writing a scratch file on {host} failed");
    }
    Ok(path)
}

/// A tunnel from local port `local` to `to` ("ip:port", as reachable from the
/// remote host). Runs until killed; the caller sets stdio and spawns it.
///
/// It is started by the forward daemon, which cannot ask for a password, so it
/// never prompts (`BatchMode`) and relies on the connection [`connect`] opened,
/// or on key or agent auth, to get through.
pub fn tunnel(remote: &str, local: u16, to: &str) -> Command {
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

/// Whether running a step a second time, after the connection dropped while it
/// ran, does no harm. A dropped connection (ssh exits 255) says nothing about
/// whether the remote command ran: it may have finished with only its reply
/// lost. Only a [`Repeat::Safe`] step is rerun; the rest report the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    /// Reads, probes and steps that leave the same state when run again
    /// (`mkdir -p`, `rm -rf`, a file replaced whole).
    Safe,
    /// Everything else: user hooks, installers, anything that creates.
    #[default]
    Never,
}

/// What a finished ssh step says about the connection.
pub trait Outcome {
    /// Whether ssh itself failed (exit 255) rather than the remote command.
    fn connection_lost(&self) -> bool;
}

impl Outcome for ExitStatus {
    fn connection_lost(&self) -> bool {
        self.code() == Some(255)
    }
}

impl Outcome for Output {
    /// A failure that trying again cannot change (bad credentials, an unknown
    /// or changed host key, a configuration ssh rejects) is not a lost
    /// connection.
    fn connection_lost(&self) -> bool {
        const PERMANENT: [&str; 4] = [
            "Permission denied",
            "Host key verification failed",
            "ControlPath too long",
            "Bad configuration option",
        ];
        let err = String::from_utf8_lossy(&self.stderr);
        self.status.connection_lost() && !PERMANENT.iter().any(|m| err.contains(m))
    }
}

/// A step that also sent data, with how sending went.
impl<T> Outcome for (ExitStatus, T) {
    fn connection_lost(&self) -> bool {
        self.0.connection_lost()
    }
}

/// How often and how fast a lost connection is retried.
struct Policy {
    tries: u32,
    first_delay: Duration,
}

const POLICY: Policy = Policy {
    tries: 3,
    first_delay: Duration::from_secs(1),
};

/// Runs `op` (one ssh invocation) and, if the connection was lost and the step
/// is [`Repeat::Safe`], drops the dead master connection and runs it again,
/// waiting 1 s, then 2 s, up to three tries. `op` is called afresh each time, so
/// it builds its command (and any stdin) anew. A step that is not safe to repeat
/// is run once and the loss reported.
pub fn retry<T: Outcome>(
    remote: &str,
    repeat: Repeat,
    op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    retry_with(&POLICY, remote, repeat, || drop_master(remote), op)
}

fn retry_with<T: Outcome>(
    policy: &Policy,
    remote: &str,
    repeat: Repeat,
    mut reset: impl FnMut(),
    mut op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut delay = policy.first_delay;
    let mut attempt = 1;
    loop {
        let out = op()?;
        if !out.connection_lost() {
            return Ok(out);
        }
        if repeat == Repeat::Never {
            crate::output::warning!(
                "lost the connection to {remote} during a step that is not safe to repeat; \
                 run the command again"
            );
            return Ok(out);
        }
        if attempt >= policy.tries {
            return Ok(out);
        }
        attempt += 1;
        crate::output::warning!(
            "lost the connection to {remote}; retrying ({attempt}/{})",
            policy.tries
        );
        std::thread::sleep(delay);
        delay *= 2;
        reset();
    }
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
pub fn connect(remote: &str) -> anyhow::Result<()> {
    let out = retry(remote, Repeat::Safe, || {
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

/// Whether the remote SSH server denies PTY allocation, which makes an
/// interactive `docker exec -it` shell impossible. Forces a PTY (`-tt`) and runs
/// `tty`: a server that refuses it makes ssh fail with "Pseudo-terminal
/// allocation request failed" (or, if it proceeds without one, `tty` prints "not
/// a tty"). Best-effort and quiet: any spawn error just yields `false`.
pub fn pty_denied(remote: &str) -> bool {
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

/// A local command line that opens an interactive session on `remote` running
/// `remote_command`, with a PTY forced (`-tt`). It is meant to be run by `sh -c`,
/// so it is a string rather than a [`Command`].
pub fn interactive_line(remote: &str, remote_command: &str) -> String {
    let (mut cmd, dest) = base(remote);
    cmd.arg("-tt").arg(dest).arg(remote_command);
    let mut line = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        line.push(' ');
        line.push_str(&crate::shell::shell_quote(&a.to_string_lossy()));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Policy, Repeat, command, interactive_line, retry_with, split, tunnel};
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::Duration;

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
        let a = args(&command("user@host:2222", "docker ps"));
        assert_eq!(&a[..2], ["-p", "2222"]);
        assert_eq!(&a[a.len() - 2..], ["user@host", "docker ps"]);
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
    fn the_interactive_line_forces_a_pty_reuses_the_connection_and_quotes_the_command() {
        let line = interactive_line("user@host:2222", "docker exec -it c sh");
        assert!(line.starts_with("ssh '-p' '2222' "), "{line}");
        assert!(line.contains("'ControlMaster=auto'"), "{line}");
        assert!(
            line.ends_with(" '-tt' 'user@host' 'docker exec -it c sh'"),
            "{line}"
        );
    }

    const FAST: Policy = Policy {
        tries: 3,
        first_delay: Duration::ZERO,
    };

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    /// Runs a step that answers with `codes` in turn; returns the last status,
    /// how often it ran and how often the connection was reset.
    fn run(repeat: Repeat, codes: &[i32]) -> (i32, usize, usize) {
        let (mut ran, mut resets) = (0, 0);
        let status = retry_with(
            &FAST,
            "h",
            repeat,
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
    fn only_ssh_exiting_255_is_a_lost_connection() {
        assert!(exit(255).connection_lost());
        assert!(!exit(0).connection_lost());
        assert!(!exit(1).connection_lost());
        assert!(
            !ExitStatus::from_raw(9).connection_lost(),
            "killed by a signal"
        );
    }

    #[test]
    fn a_refused_login_is_not_a_lost_connection() {
        let out = |code, err: &str| std::process::Output {
            status: exit(code),
            stdout: vec![],
            stderr: err.as_bytes().to_vec(),
        };
        assert!(out(255, "ssh: connect to host h port 22: Connection refused").connection_lost());
        assert!(!out(255, "u@h: Permission denied (publickey).").connection_lost());
        assert!(!out(255, "Host key verification failed.").connection_lost());
        assert!(!out(255, "ControlPath too long ('/x' >= 108 bytes)").connection_lost());
        assert!(!out(1, "Connection refused").connection_lost());
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
}
