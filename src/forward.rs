use crate::output::{info, scoped, warning};
use std::collections::{HashMap, HashSet};

use crate::runtime::{self, Container, ExecOpts};
use crate::target::Target;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

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

/// Port-forwarding daemon. Forwards declared in `forwardPorts`
/// (`static_ports`) are set up eagerly, from startup — connections just fail
/// until the service listens. This also covers containers whose listeners
/// can't be inspected (no shell for `cat /proc/net/tcp`) and targets other
/// than the primary container (compose services). On top of that, the daemon
/// polls `/proc/net/tcp` and `/proc/net/tcp6` every 2 s and maintains one
/// forwarder per listening port. For local Docker, a built-in Rust TCP proxy
/// is used; for `--remote`, an `ssh -L` tunnel. Exits when the container stops.
pub fn run_daemon(container_id: &str, target: &Target, static_ports: &[PortForward]) {
    // Every `ldg up` starts a daemon; only one per container may run, or they
    // fight over the same local ports. The lock lives as long as this process.
    let _lock = match acquire_lock(&lock_path(container_id)) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            scoped!(
                "lodge forward",
                "already running for this container; exiting"
            );
            return;
        }
        Err(e) => {
            scoped!(
                "lodge forward",
                "could not take the forwarder lock ({e}); continuing without it"
            );
            None
        }
    };
    let rt = runtime::connect(target);
    let container = rt.container(container_id);
    let container = container.as_ref();
    let container_ip = match container.ip_address() {
        Some(ip) => ip,
        None => {
            scoped!(
                "lodge forward",
                "could not determine container IP — port forwarding disabled"
            );
            return;
        }
    };
    scoped!("lodge forward", "container IP: {container_ip}");

    let mut active: HashMap<u16, Forwarder> = HashMap::new();
    // Local ports whose forwarder failed to start, so the retry every 2 s
    // (likely failing the same way, e.g. host port occupied) doesn't spam the
    // log. Same warn-once treatment for named hosts that don't resolve (the
    // target service may simply not be up yet — keep trying quietly).
    let mut failed: HashSet<u16> = HashSet::new();
    let mut unresolved: HashSet<String> = HashSet::new();
    // Successful name → IP resolutions, so steady state needs no docker calls.
    let mut resolved: HashMap<String, String> = HashMap::new();

    loop {
        if !container.is_running() {
            scoped!("lodge forward", "container stopped, exiting");
            break;
        }

        // Wanted forwards, keyed by local port: every listening port maps to
        // itself on the primary container; declared forwards are always wanted
        // and win a local-port collision. A forwarder that died (e.g. dropped
        // ssh tunnel) is retired below and recreated, since its local port
        // stays in the wanted set.
        let mut wanted: HashMap<u16, PortForward> = poll_listening_ports(container)
            .into_iter()
            .map(|p| (p, PortForward::same(p)))
            .collect();
        for f in static_ports {
            wanted.insert(f.local, f.clone());
        }

        // Drop forwarders for ports that stopped listening or whose process died.
        active.retain(|&local, fwd| {
            let alive = wanted.contains_key(&local) && fwd.is_alive();
            if !alive {
                scoped!("lodge forward", "stopped forwarding port {local}");
            }
            alive
        });

        // Start forwarders for wanted ports that don't have one yet.
        for (&local, f) in &wanted {
            if active.contains_key(&local) {
                continue;
            }
            let target_ip = match &f.host {
                None => container_ip.clone(),
                Some(h) => match resolved.get(h) {
                    Some(ip) => ip.clone(),
                    None => match container.resolve_peer(h) {
                        Some(ip) => {
                            scoped!("lodge forward", "resolved {h} → {ip}");
                            resolved.insert(h.clone(), ip.clone());
                            unresolved.remove(h);
                            ip
                        }
                        None => {
                            if unresolved.insert(h.clone()) {
                                scoped!(
                                    "lodge forward",
                                    "cannot resolve '{h}' on the container \
                                     network (service not up yet?) — will keep trying"
                                );
                            }
                            continue;
                        }
                    },
                },
            };
            let dest = format!("{target_ip}:{}", f.port);
            let fwd = if let Some(r) = target.ssh_host() {
                scoped!("lodge forward", "port {local} — ssh -L {local}:{dest} {r}");
                Forwarder::ssh_tunnel(local, &dest, r)
            } else {
                scoped!(
                    "lodge forward",
                    "port {local} — proxy 127.0.0.1:{local} → {dest}"
                );
                Forwarder::proxy(local, dest)
            };
            match fwd {
                Ok(fw) => {
                    scoped!("lodge forward", "forwarding port {local}");
                    active.insert(local, fw);
                    failed.remove(&local);
                }
                Err(e) => {
                    if failed.insert(local) {
                        scoped!("lodge forward", "could not forward port {local}: {e}");
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_secs(2));
    }
}

// ── Forwarder ─────────────────────────────────────────────────────────────────

enum Forwarder {
    Proxy(ProxyHandle),
    Ssh(Child),
}

impl Forwarder {
    // `target` is "ip:port" as reachable from the Docker host.
    fn proxy(local: u16, target: String) -> std::io::Result<Self> {
        ProxyHandle::spawn(local, target).map(Forwarder::Proxy)
    }

    fn ssh_tunnel(local: u16, target: &str, remote: &str) -> std::io::Result<Self> {
        let mut cmd = crate::ssh::tunnel(remote, local, target);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            // With --verbose, let ssh's own chatter land in the daemon log.
            .stderr(if crate::output::is_verbose() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        crate::output::trace(&cmd);
        cmd.spawn().map(Forwarder::Ssh)
    }

    fn is_alive(&mut self) -> bool {
        match self {
            Forwarder::Proxy(h) => !h.stop.load(Ordering::Relaxed),
            // A tunnel we can't query is treated as dead, so it is replaced.
            Forwarder::Ssh(child) => child.try_wait().map(|s| s.is_none()).unwrap_or(false),
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        match self {
            Forwarder::Proxy(h) => h.stop.store(true, Ordering::Relaxed),
            Forwarder::Ssh(child) => {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the tunnel may already have exited"
                )]
                let _ = child.kill();
            }
        }
    }
}

// ── Built-in TCP proxy ────────────────────────────────────────────────────────

struct ProxyHandle {
    stop: Arc<AtomicBool>,
}

impl ProxyHandle {
    // `target` is "ip:port" as reachable from the Docker host.
    fn spawn(local: u16, target: String) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", local))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        std::thread::spawn(move || {
            loop {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((client, _)) => {
                        let target = target.clone();
                        std::thread::spawn(move || {
                            if let Ok(server) = TcpStream::connect(&target) {
                                proxy_conn(client, server);
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ProxyHandle { stop })
    }
}

// Bidirectional copy between two TCP streams. Runs client→server in a spawned
// thread and server→client in the current thread; each half shuts down its write
// end on EOF so the peer sees a clean close.
fn proxy_conn(client: TcpStream, server: TcpStream) {
    let mut c_r = client;
    let mut s_r = server;
    let mut s_w = match s_r.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut c_w = match c_r.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };

    // A reset or already-closed peer is the normal end of a proxied connection,
    // not an error worth reporting, so copy results are ignored below.
    let t = std::thread::spawn(move || {
        std::io::copy(&mut c_r, &mut s_w).ok();
        s_w.shutdown(std::net::Shutdown::Write).ok();
    });

    std::io::copy(&mut s_r, &mut c_w).ok();
    c_w.shutdown(std::net::Shutdown::Write).ok();
    t.join().ok();
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn state_path(container: &str, extension: &str) -> PathBuf {
    let short = container.get(..12).unwrap_or(container);
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("lodge")
        .join(format!("forward-{short}.{extension}"))
}

fn log_path(container: &str) -> PathBuf {
    state_path(container, "log")
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

fn poll_listening_ports(container: &dyn Container) -> HashSet<u16> {
    // The exit status is ignored on purpose: `cat` fails when there is no
    // /proc/net/tcp6 (IPv6 disabled) but has still printed the IPv4 listeners.
    container
        .output(
            &["sh", "-c", "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null"],
            &ExecOpts::default(),
        )
        .map(|out| parse_proc_net_tcp(&out.stdout))
        .unwrap_or_default()
}

// /proc/net/tcp format (one header line, then one connection per line):
//   sl  local_address rem_address st ...
//   0:  00000000:1F90 00000000:0000 0A ...
// The port is the hex value after the colon in local_address; state 0A = LISTEN.
// tcp6 uses a 32-char hex address but the same port encoding. Loopback-only
// listeners are skipped: they're unreachable via the container IP, and every
// container on a user-defined network has one (Docker's embedded DNS resolver
// on 127.0.0.11) that would otherwise be forwarded as noise.
fn parse_proc_net_tcp(data: &str) -> HashSet<u16> {
    let mut ports = HashSet::new();
    for line in data.lines() {
        let mut cols = line.split_whitespace();
        cols.next(); // sl / line index
        let local = match cols.next() {
            Some(v) => v,
            None => continue,
        };
        cols.next(); // rem_address
        let state = match cols.next() {
            Some(v) => v,
            None => continue,
        };
        if state != "0A" {
            continue;
        }
        let mut parts = local.split(':');
        let (Some(addr), Some(hex)) = (parts.next(), parts.next()) else {
            continue;
        };
        if is_loopback_hex(addr) {
            continue;
        }
        if let Ok(port) = u16::from_str_radix(hex, 16)
            && port > 0
        {
            ports.insert(port);
        }
    }
    ports
}

// True if a /proc/net/tcp{,6} hex address is a loopback address. IPv4 is one
// little-endian u32 ("0100007F" = 127.0.0.1, "0B00007F" = 127.0.0.11); IPv6 is
// four such u32 groups, with ::1 being all-zero except a final "01000000".
fn is_loopback_hex(addr: &str) -> bool {
    match addr.len() {
        8 => u32::from_str_radix(addr, 16)
            .map(|v| std::net::Ipv4Addr::from(v.swap_bytes()).is_loopback())
            .unwrap_or(false),
        32 => addr.eq_ignore_ascii_case("00000000000000000000000001000000"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_ports() {
        let data = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0
   1: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0
   2: 0F02000A:C4E2 0202000A:1F90 01 00000000:00000000 00:00000000 00000000  1000
";
        let ports = parse_proc_net_tcp(data);
        assert!(ports.contains(&8080), "0x1F90 = 8080");
        assert!(ports.contains(&80), "0x0050 = 80");
        assert!(!ports.contains(&50402), "ESTABLISHED, not LISTEN");
    }

    #[test]
    fn parses_ipv6_listen_ports() {
        let data = "\
  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A
";
        let ports = parse_proc_net_tcp(data);
        assert!(ports.contains(&8080));
    }

    #[test]
    fn ignores_port_zero() {
        let data = "  sl local rem st\n   0: 00000000:0000 00000000:0000 0A\n";
        assert!(parse_proc_net_tcp(data).is_empty());
    }

    #[test]
    fn skips_loopback_listeners() {
        // 127.0.0.1:8080 (v4), 127.0.0.11:53 (docker DNS), ::1:9090 (v6) must
        // all be skipped; 0.0.0.0:8081 and [::]:9091 must be kept.
        let data = "\
  sl  local_address rem_address   st
   0: 0100007F:1F90 00000000:0000 0A
   1: 0B00007F:0035 00000000:0000 0A
   2: 00000000:1F91 00000000:0000 0A
   3: 00000000000000000000000001000000:2382 00000000000000000000000000000000:0000 0A
   4: 00000000000000000000000000000000:2383 00000000000000000000000000000000:0000 0A
";
        let ports = parse_proc_net_tcp(data);
        assert_eq!(ports, HashSet::from([8081, 9091]));
    }

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

    #[test]
    fn the_proxy_listens_on_localhost_only() {
        // Port 0 lets the OS pick; the point is the address family/interface.
        let handle = ProxyHandle::spawn(0, "127.0.0.1:9".into());
        // spawn() binds the requested port; with 0 it still binds loopback.
        assert!(handle.is_ok());
        let src = include_str!("forward.rs");
        assert!(
            !src.contains(concat!("bind(format!(\"0.0", ".0.0")),
            "the proxy must not bind all interfaces"
        );
    }
}
