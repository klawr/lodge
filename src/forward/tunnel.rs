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
    /// Nothing to run: what is published on this machine is already there, on
    /// the port it was asked for.
    Direct {
        local: u16,
        dest: String,
    },
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

    /// The container port a relay behind it forwards to, `None` when there is no
    /// relay: a forward whose relay aims elsewhere is not the one that is wanted.
    fn relays(&self) -> Option<u16> {
        None
    }
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

    /// Whether the machine the forwards run on waits for a login the daemon
    /// cannot give, so that being unable to reach it is no reason to stop.
    fn awaiting_login(&self) -> bool {
        false
    }
}

/// Where a host's own processes put their errors: in the daemon log with
/// `--verbose`, nowhere otherwise.
pub fn chatter() -> Stream {
    if crate::output::is_verbose() {
        Stream::Inherit
    } else {
        Stream::Null
    }
}

/// A forward in front of a relay process the container's engine host runs for the
/// port (`kubectl port-forward`): it is alive while both are, and stopping it
/// stops both.
pub struct Relayed {
    relay: Box<dyn Child>,
    port: u16,
    inner: Box<dyn Forward>,
    /// When the relay was started, as for a tunnel.
    since: Instant,
}

impl Relayed {
    /// `inner` forwards to where `relay` listens; `relay` relays to `port` of the
    /// container.
    pub fn new(relay: Box<dyn Child>, port: u16, inner: Box<dyn Forward>) -> Self {
        Self {
            relay,
            port,
            inner,
            since: Instant::now(),
        }
    }
}

impl Forward for Relayed {
    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn dest(&self) -> &str {
        self.inner.dest()
    }

    fn ran_for(&self) -> Option<Duration> {
        Some(self.since.elapsed())
    }

    fn is_alive(&mut self) -> bool {
        self.relay.try_wait().map(|s| s.is_none()).unwrap_or(false) && self.inner.is_alive()
    }

    fn relays(&self) -> Option<u16> {
        Some(self.port)
    }
}

impl Drop for Relayed {
    fn drop(&mut self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the relay may already have exited"
        )]
        let _ = self.relay.kill();
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
        let chatter = chatter();
        let forward = if let Some(child) = self.host.tunnel(local, dest, chatter) {
            log!("port {local} — tunnel to {dest}{}", self.on_host());
            Forwarder::tunnel(child?, local, dest)
        } else if dest == format!("127.0.0.1:{local}") {
            // The same machine, the same port: a proxy would only fight the
            // listener that is already there for the port.
            log!("port {local} — already reachable on this machine");
            Forwarder::Direct {
                local,
                dest: dest.to_string(),
            }
        } else {
            log!("port {local} — proxy 127.0.0.1:{local} → {dest}");
            Forwarder::proxy(local, dest.to_string())?
        };
        Ok(Box::new(forward))
    }

    fn on_host(&self) -> String {
        self.host.on_host()
    }

    fn awaiting_login(&self) -> bool {
        self.host.awaiting_login()
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
            Forwarder::Direct { local, dest } => format!("{local}\t{dest}"),
            Forwarder::Proxy(h) => format!("{}\t{}", h.bound.port(), h.dest),
            Forwarder::Tunnel { local, dest, .. } => format!("{local}\t{dest}"),
        }
    }

    fn dest(&self) -> &str {
        match self {
            Forwarder::Direct { dest, .. } => dest,
            Forwarder::Proxy(h) => &h.dest,
            Forwarder::Tunnel { dest, .. } => dest,
        }
    }

    fn ran_for(&self) -> Option<Duration> {
        match self {
            Forwarder::Direct { .. } | Forwarder::Proxy(_) => None,
            Forwarder::Tunnel { since, .. } => Some(since.elapsed()),
        }
    }

    fn is_alive(&mut self) -> bool {
        match self {
            Forwarder::Direct { .. } => true,
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
            Forwarder::Direct { .. } => {}
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
    fn the_forwarders_say_whether_their_host_waits_for_a_login() {
        let waiting = crate::host::fake::FakeHost::remote("h").awaiting_login();
        assert!(HostForwarders::new(&waiting).awaiting_login());
        let fine = crate::host::fake::FakeHost::remote("h");
        assert!(!HostForwarders::new(&fine).awaiting_login());
    }

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
    fn what_this_machine_already_has_on_the_port_is_not_proxied_over_itself() {
        let host = crate::host::fake::FakeHost::new();
        let mut forward = HostForwarders::new(&host)
            .start(13000, "127.0.0.1:13000")
            .unwrap();
        assert_eq!(forward.describe(), "13000\t127.0.0.1:13000");
        assert!(forward.is_alive() && forward.ran_for().is_none());
        assert!(host.argvs().is_empty());
        // Another machine still tunnels the same port, to its own loopback.
        let remote = crate::host::fake::FakeHost::remote("h").with_tunnels();
        HostForwarders::new(&remote)
            .start(13000, "127.0.0.1:13000")
            .unwrap();
        assert_eq!(remote.argvs(), [["tunnel", "13000", "127.0.0.1:13000"]]);
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
