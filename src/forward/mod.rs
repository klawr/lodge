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

pub use daemon::{UNREACHABLE_POLLS, run_daemon};
pub use tunnel::HostForwarders;

use crate::port::PortForward;

/// Starts the port forwarding for a container. A trait so that `ldg up` does not
/// start a real process in a test, and so another way of forwarding is one more
/// implementation.
pub trait ForwardDaemon {
    /// Starts forwarding for `container` and returns at once; what it does with
    /// `static_ports` (the declared forwards) is its own business.
    fn start(&self, container: &str, static_ports: &[PortForward]);

    /// Ends the forwarding for `container`, if there is any, and returns at once.
    /// What has no end of its own (a machine that is always up) is ended only by
    /// this.
    fn stop(&self, container: &str);
}

/// Spawns a detached `ldg _forward` daemon per container. It outlives the
/// calling process; its stderr goes to a log file whose path is printed so the
/// user can tail it.
pub struct DetachedDaemon {
    /// The arguments that point a child `ldg` at the same host and engine as this
    /// process.
    args: Vec<String>,
}

impl DetachedDaemon {
    /// `args` point the child at the same host and engine as this process.
    pub fn new(args: Vec<String>) -> Self {
        DetachedDaemon { args }
    }
}

impl ForwardDaemon for DetachedDaemon {
    fn start(&self, container: &str, static_ports: &[PortForward]) {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                warning!("cannot start port forwarding: {e}");
                return;
            }
        };

        let log = match log_path(container) {
            Ok(l) => l,
            Err(e) => {
                warning!("cannot start port forwarding: {e}");
                return;
            }
        };
        let log_file = match open_log(&log) {
            Ok(f) => f,
            Err(e) => {
                warning!("could not open forward log {}: {e}", log.display());
                return;
            }
        };

        info!("port forwarding log: {}", log.display());

        let mut cmd = Command::new(exe);
        cmd.args(&self.args);
        // Forward verbosity to the daemon so its tunnels log into the log file.
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

    fn stop(&self, container: &str) {
        if let Err(e) = Self::ask_to_stop(container) {
            warning!("could not stop the port-forwarding daemon: {e}");
        }
    }
}

impl DetachedDaemon {
    /// Asks the daemon for `container` to end, by leaving a note it looks for
    /// between polls. Only when one is running: a note left for none would end the
    /// next one at birth.
    fn ask_to_stop(container: &str) -> std::io::Result<()> {
        if active_ports(container)?.is_some() {
            std::fs::write(stop_path(container)?, b"")?;
        }
        Ok(())
    }
}

/// Writes the ports file: the daemon's pid, then one `local<TAB>dest` per
/// active forward. Written beside it and renamed into place, so `ldg ports`
/// never reads half a file.
fn write_ports(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut text = format!("pid {}\n", std::process::id());
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    let partial = path.with_extension("ports.partial");
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, path)
}

/// Empties the log of the daemon for `container`. Called by the daemon once it
/// holds the lock, so a new daemon starts a new log and a daemon that finds
/// one already running leaves that one's log alone.
fn truncate_log(container: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(log_path(container)?)?
        .set_len(0)
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
/// An error only when there is nowhere the daemon could keep its state.
pub fn active_ports(container_id: &str) -> std::io::Result<Option<Vec<ActivePort>>> {
    let Ok(text) = std::fs::read_to_string(ports_path(container_id)?) else {
        return Ok(None);
    };
    Ok(parse_ports(&text, is_daemon))
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

/// `~/.local/share/lodge/forward-<id>.<extension>`; an error when this system
/// has no data directory (rather than a guess relative to wherever lodge runs).
fn state_path(container: &str, extension: &str) -> std::io::Result<PathBuf> {
    let short = container.get(..12).unwrap_or(container);
    let dir = dirs::data_local_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "this system has no data directory for the port-forwarding state",
        )
    })?;
    Ok(dir
        .join("lodge")
        .join(format!("forward-{short}.{extension}")))
}

/// The port-forward daemon's log for `container`.
pub fn log_path(container: &str) -> std::io::Result<PathBuf> {
    state_path(container, "log")
}

fn ports_path(container: &str) -> std::io::Result<PathBuf> {
    state_path(container, "ports")
}

/// The note `ldg` leaves for the daemon of `container` to end.
fn stop_path(container: &str) -> std::io::Result<PathBuf> {
    state_path(container, "stop")
}

/// Whether a stop was asked for, taking the note away so it is answered once.
fn stop_requested(path: &std::path::Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

fn lock_path(container: &str) -> std::io::Result<PathBuf> {
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
    fn a_request_to_stop_is_answered_once() {
        let dir = crate::tmpfile::TempDir::new("stop").unwrap();
        let note = dir.path().join("forward-x.stop");
        assert!(!stop_requested(&note), "nothing asked");
        std::fs::write(&note, b"").unwrap();
        assert!(stop_requested(&note));
        assert!(!stop_requested(&note), "the note is taken away");
    }

    #[test]
    fn the_ports_file_is_replaced_whole() {
        let dir = crate::tmpfile::TempDir::new("ports").unwrap();
        let path = dir.path().join("forward-x.ports");
        write_ports(&path, &["8080\t1.2.3.4:80".into()]).unwrap();
        write_ports(&path, &[]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("pid {}\n", std::process::id()));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no leftovers"
        );
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
