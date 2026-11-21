//! One forward: the built-in TCP proxy (the engine's addresses are reachable
//! from here) or the tunnel process the host provides (they are not).

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::host::{Child, Host, Stream};

macro_rules! log {
    ($($arg:tt)*) => { $crate::output::scoped!("lodge forward", $($arg)*) };
}

// ── Forwarder ─────────────────────────────────────────────────────────────────

enum Forwarder {
    Proxy(ProxyHandle),
    Tunnel {
        child: Box<dyn Child>,
        local: u16,
        dest: String,
        /// When it was started, to tell a tunnel that held for a while from one
        /// that died at once.
        since: Instant,
    },
}

/// One running forward. Dropping it stops it.
pub trait Forward {
    /// `local<TAB>dest`, for the ports file.
    fn describe(&self) -> String;

    /// Where it forwards to: `ip:port` as reachable from the engine's host.
    fn dest(&self) -> &str;

    /// How long a host tunnel has been running, `None` for one with no
    /// connection to lose (the in-process proxy).
    fn ran_for(&self) -> Option<Duration>;

    /// Whether it still forwards. One that can't be asked counts as dead, so it
    /// is replaced.
    fn is_alive(&mut self) -> bool;
}

/// Starts forwards from a local port to an address on the engine's network.
pub trait Forwarders {
    /// Forwards `127.0.0.1:local` to `dest` (`ip:port`, reachable from the
    /// engine's host).
    fn start(&self, local: u16, dest: &str) -> std::io::Result<Box<dyn Forward>>;

    /// `" on <host>"` for a forward that runs on another machine, for the log.
    fn on_host(&self) -> String {
        String::new()
    }
}

/// The forwards a [`Host`] provides: its own tunnel process when what the engine
/// reaches is not reachable from here (`ssh -L`), else the built-in proxy.
pub struct HostForwarders<'a> {
    host: &'a dyn Host,
}

impl<'a> HostForwarders<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        Self { host }
    }
}

impl Forwarders for HostForwarders<'_> {
    fn start(&self, local: u16, dest: &str) -> std::io::Result<Box<dyn Forward>> {
        // With --verbose, the tunnel's own chatter lands in the daemon log.
        let chatter = if crate::output::is_verbose() {
            Stream::Inherit
        } else {
            Stream::Null
        };
        let forward = if let Some(child) = self.host.tunnel(local, dest, chatter) {
            log!("port {local} — tunnel to {dest}{}", self.on_host());
            Forwarder::tunnel(child?, local, dest)
        } else {
            log!("port {local} — proxy 127.0.0.1:{local} → {dest}");
            Forwarder::proxy(local, dest.to_string())?
        };
        Ok(Box::new(forward))
    }

    fn on_host(&self) -> String {
        self.host.on_host()
    }
}

impl Forwarder {
    // `dest` is "ip:port" as reachable from the engine's host.
    fn proxy(local: u16, dest: String) -> std::io::Result<Self> {
        ProxyHandle::spawn(local, dest).map(Forwarder::Proxy)
    }

    /// `child` is the host's own tunnel process for `local` to `dest`.
    fn tunnel(child: Box<dyn Child>, local: u16, dest: &str) -> Self {
        Forwarder::Tunnel {
            child,
            local,
            dest: dest.to_string(),
            since: Instant::now(),
        }
    }
}

impl Forward for Forwarder {
    fn describe(&self) -> String {
        match self {
            Forwarder::Proxy(h) => format!("{}\t{}", h.bound.port(), h.dest),
            Forwarder::Tunnel { local, dest, .. } => format!("{local}\t{dest}"),
        }
    }

    fn dest(&self) -> &str {
        match self {
            Forwarder::Proxy(h) => &h.dest,
            Forwarder::Tunnel { dest, .. } => dest,
        }
    }

    fn ran_for(&self) -> Option<Duration> {
        match self {
            Forwarder::Proxy(_) => None,
            Forwarder::Tunnel { since, .. } => Some(since.elapsed()),
        }
    }

    fn is_alive(&mut self) -> bool {
        match self {
            Forwarder::Proxy(h) => !h.stop.load(Ordering::Relaxed),
            Forwarder::Tunnel { child, .. } => {
                child.try_wait().map(|s| s.is_none()).unwrap_or(false)
            }
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        match self {
            Forwarder::Proxy(h) => h.stop.store(true, Ordering::Relaxed),
            Forwarder::Tunnel { child, .. } => {
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

pub(super) struct ProxyHandle {
    stop: Arc<AtomicBool>,
    /// The address actually listened on: loopback only, never all interfaces.
    bound: SocketAddr,
    /// Where it forwards to: "ip:port" as reachable from the engine's host.
    dest: String,
}

impl ProxyHandle {
    fn spawn(local: u16, dest: String) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, local))?;
        let bound = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let shown = dest.clone();

        std::thread::spawn(move || {
            loop {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((client, _)) => {
                        let dest = dest.clone();
                        std::thread::spawn(move || {
                            if let Ok(server) = TcpStream::connect(&dest) {
                                proxy_conn(client, server);
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => {
                        // Report the proxy dead so the daemon replaces it.
                        stop_thread.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });

        Ok(ProxyHandle {
            stop,
            bound,
            dest: shown,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_with_tunnels_gets_a_tunnel_that_stops_when_dropped() {
        let host = crate::host::fake::FakeHost::remote("h").with_tunnels();
        let forwarders = HostForwarders::new(&host);
        let mut forward = forwarders.start(8080, "10.0.0.2:80").unwrap();
        assert_eq!(forward.describe(), "8080\t10.0.0.2:80");
        assert!(forward.is_alive());
        assert!(
            forward.ran_for().is_some(),
            "a tunnel has a connection to lose"
        );
        assert_eq!(host.argvs(), [["tunnel", "8080", "10.0.0.2:80"]]);
        drop(forward);
    }

    #[test]
    fn a_host_without_tunnels_is_proxied_in_process() {
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dest = server.local_addr().unwrap().to_string();
        let host = crate::host::fake::FakeHost::new();
        let forward = HostForwarders::new(&host).start(0, &dest).unwrap();
        assert_eq!(forward.dest(), dest);
        assert!(forward.ran_for().is_none());
        assert!(
            host.argvs().is_empty(),
            "no process: the proxy is in-process"
        );
    }

    #[test]
    fn the_proxy_listens_on_localhost_only_and_forwards() {
        use std::io::{Read, Write};
        // A server for the proxy to forward to.
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dest = server.local_addr().unwrap().to_string();
        let echo = std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });

        // Port 0 lets the OS pick; what matters is the interface.
        let handle = ProxyHandle::spawn(0, dest).unwrap();
        assert!(handle.bound.ip().is_loopback(), "{}", handle.bound);

        let mut client = TcpStream::connect(handle.bound).unwrap();
        client.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        client.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"ping");
        echo.join().unwrap();
        handle.stop.store(true, Ordering::Relaxed);
    }
}
