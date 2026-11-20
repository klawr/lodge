//! Port forwarding from the container to `127.0.0.1` here.
//!
//! `ldg up` starts a detached `ldg _forward` [`daemon`] per container; each
//! forward it keeps is a [`tunnel`]. The daemon's log, lock and list of active
//! forwards live in `~/.local/share/lodge/forward-<id>.*`.

mod daemon;
mod tunnel;

use crate::output::{info, warning};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::target::Target;

pub use daemon::run_daemon;

/// One port forward: local port `local` on the host tunnels to port `port` of
/// a target on the container network — the primary container itself when
/// `host` is None, or a named host (compose service, container name, network
/// alias) otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub local: u16,
    pub host: Option<String>,
    pub port: u16,
}

impl PortForward {
    /// Local port == target port on the primary container (the common case).
    pub fn same(port: u16) -> Self {
        Self {
            local: port,
            host: None,
            port,
        }
    }

    /// Serialized form for the internal `ldg _forward --port` flag.
    pub fn encode(&self) -> String {
        match &self.host {
            Some(h) => format!("{}:{}:{}", self.local, h, self.port),
            None => format!("{}:{}", self.local, self.port),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            [l, p] => Some(Self {
                local: l.parse().ok()?,
                host: None,
                port: p.parse().ok()?,
            }),
            [l, h, p] => Some(Self {
                local: l.parse().ok()?,
                host: Some(h.to_string()),
                port: p.parse().ok()?,
            }),
            _ => None,
        }
    }
}

/// Spawns a detached `ldg _forward` daemon that manages port forwarding for
/// `container`. Returns immediately; the daemon outlives the calling process.
/// Daemon stderr goes to a log file; the path is printed so the user can tail it.
pub fn spawn_daemon(container: &str, target: &Target, static_ports: &[PortForward]) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            warning!("cannot start port forwarding: {e}");
            return;
        }
    };

    let log = log_path(container);
    let log_file = match open_log(&log) {
        Ok(f) => f,
        Err(e) => {
            warning!("could not open forward log {}: {e}", log.display());
            return;
        }
    };

    info!("port forwarding log: {}", log.display());

    let mut cmd = Command::new(exe);
    if let Some(host) = target.ssh_host() {
        cmd.args(["--remote", host]);
    }
    // Forward verbosity to the daemon so its ssh tunnels log into the log file.
    for _ in 0..crate::output::verbosity() {
        cmd.arg("-v");
    }
    cmd.arg("_forward").arg(container);
    for p in static_ports {
        cmd.args(["--port", &p.encode()]);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    if let Err(e) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
    {
        warning!("could not start the port-forwarding daemon: {e}");
    }
}

/// Writes the ports file: the daemon's pid, then one `local<TAB>dest` per
/// active forward.
fn write_ports(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut text = format!("pid {}\n", std::process::id());
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    std::fs::write(path, text)
}

/// One forward the daemon reports as active.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActivePort {
    /// The port on 127.0.0.1 of this machine.
    pub local: u16,
    /// Where it leads, as `ip:port` seen from the engine's host.
    pub dest: String,
}

/// The forwards the daemon for `container` has active, if it is running. `None`
/// when no daemon is running (none started, or the one that wrote the file died).
pub fn active_ports(container_id: &str) -> Option<Vec<ActivePort>> {
    parse_ports(
        &std::fs::read_to_string(ports_path(container_id)).ok()?,
        is_daemon,
    )
}

/// Whether `pid` is a running `ldg _forward`. Read from /proc rather than by
/// probing the daemon's lock, which could make a starting daemon think another
/// one is running.
fn is_daemon(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|c| c.split(|&b| b == 0).any(|a| a == b"_forward"))
}

fn parse_ports(text: &str, alive: impl Fn(u32) -> bool) -> Option<Vec<ActivePort>> {
    let mut lines = text.lines();
    let pid = lines.next()?.strip_prefix("pid ")?.trim().parse().ok()?;
    if !alive(pid) {
        return None;
    }
    Some(
        lines
            .filter_map(|l| {
                let (local, dest) = l.split_once('\t')?;
                Some(ActivePort {
                    local: local.parse().ok()?,
                    dest: dest.to_string(),
                })
            })
            .collect(),
    )
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn state_path(container: &str, extension: &str) -> PathBuf {
    let short = container.get(..12).unwrap_or(container);
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("lodge")
        .join(format!("forward-{short}.{extension}"))
}

/// The port-forward daemon's log for `container`.
pub fn log_path(container: &str) -> PathBuf {
    state_path(container, "log")
}

fn ports_path(container: &str) -> PathBuf {
    state_path(container, "ports")
}

fn lock_path(container: &str) -> PathBuf {
    state_path(container, "lock")
}

/// Takes an exclusive lock on `path`: `Ok(Some(_))` if taken, `Ok(None)` if
/// another process holds it, `Err` if the lock file can't be used. The lock is released when the returned file is dropped or the process
/// exits, however it exits, so a crashed daemon never leaves a stale lock.
fn acquire_lock(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Opens the daemon's log file for appending, creating its directory.
fn open_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_forward_encode_decode_roundtrip() {
        for f in [
            PortForward::same(8080),
            PortForward {
                local: 3000,
                host: None,
                port: 8080,
            },
            PortForward {
                local: 5432,
                host: Some("db".into()),
                port: 5432,
            },
        ] {
            assert_eq!(PortForward::decode(&f.encode()), Some(f));
        }
        assert_eq!(PortForward::decode("garbage"), None);
        assert_eq!(PortForward::decode("1:2:3:4"), None);
    }

    #[test]
    fn the_ports_file_counts_only_while_its_daemon_lives() {
        let text = "pid 42\n8080\t172.17.0.2:8080\n";
        let ports = parse_ports(text, |pid| pid == 42).unwrap();
        assert_eq!(
            ports,
            [ActivePort {
                local: 8080,
                dest: "172.17.0.2:8080".into()
            }]
        );
        assert!(parse_ports(text, |_| false).is_none());
        assert!(parse_ports("8080\tx\n", |_| true).is_none());
        assert_eq!(parse_ports("pid 1\n", |_| true), Some(vec![]));
    }

    #[test]
    fn only_one_holder_of_the_lock_at_a_time() {
        let dir = crate::tmpfile::TempDir::new("locktest").unwrap();
        let path = dir.path().join("forward.lock");
        let first = acquire_lock(&path)
            .unwrap()
            .expect("first acquisition succeeds");
        assert!(
            acquire_lock(&path).unwrap().is_none(),
            "second is refused while held"
        );
        drop(first);
        assert!(
            acquire_lock(&path).unwrap().is_some(),
            "free again once released"
        );
    }
}
