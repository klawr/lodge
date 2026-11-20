//! Everything about `ssh`: parsing a `--remote` destination, connection
//! multiplexing, and the few ssh invocations lodge needs. Every ssh command is
//! built from [`base`] here, so port, multiplexing and `-v` are applied once.

use anyhow::Context as _;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
/// is traced at `-v`.
pub fn sh_output(host: &str, script: &str) -> anyhow::Result<std::process::Output> {
    let mut cmd = command(
        host,
        &format!("sh -c {}", crate::shell::shell_quote(script)),
    );
    crate::output::trace(&cmd);
    let out = cmd.output().context("spawning ssh")?;
    // With ssh -v the debug chatter lands on stderr; surface it.
    if crate::output::is_verbose() && !out.stderr.is_empty() {
        crate::output::raw_stderr(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(out)
}

/// Writes `content` to `path` on the remote host.
pub fn write_file(host: &str, path: &str, content: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut cmd = command(host, &format!("cat > {}", crate::shell::shell_quote(path)));
    cmd.stdin(Stdio::piped()).stdout(Stdio::null());
    crate::output::trace(&cmd);
    let mut child = cmd.spawn().context("spawning ssh")?;
    child
        .stdin
        .take()
        .context("ssh stdin was not piped")?
        .write_all(content.as_bytes())
        .context("writing to ssh stdin")?;
    let status = child.wait().context("waiting for ssh")?;
    if !status.success() {
        anyhow::bail!("writing {path} on {host} failed");
    }
    Ok(())
}

/// A tunnel from local port `local` to `target` ("ip:port", as reachable from
/// the remote host). Runs until killed; the caller sets stdio and spawns it.
pub fn tunnel(remote: &str, local: u16, target: &str) -> Command {
    let (mut c, dest) = base(remote);
    c.args([
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-L",
        &format!("{local}:{target}"),
    ])
    .arg(dest);
    c
}

/// Whether the remote SSH server denies PTY allocation, which makes an
/// interactive `docker exec -it` shell impossible. Forces a PTY (`-tt`) and runs
/// `tty`: a server that refuses it makes ssh fail with "Pseudo-terminal
/// allocation request failed" (or, if it proceeds without one, `tty` prints "not
/// a tty"). Best-effort and quiet: any spawn error just yields `false`.
pub fn pty_denied(remote: &str) -> bool {
    let (mut cmd, dest) = base(remote);
    cmd.args(["-tt", "-o", "BatchMode=yes"])
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
/// `remote_command`, with a PTY forced (`-tt`). It is meant to be run by `sh -c`
/// or a tmux pane, so it is a string rather than a [`Command`].
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
    use super::{command, interactive_line, split, tunnel};

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
}
