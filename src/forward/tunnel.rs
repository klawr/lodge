//! One forward: the built-in TCP proxy (local engine) or an `ssh -L` tunnel
//! (`--remote`).

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

// ── Forwarder ─────────────────────────────────────────────────────────────────

pub(super) enum Forwarder {
    Proxy(ProxyHandle),
    Ssh {
        child: Child,
        local: u16,
        dest: String,
    },
}

impl Forwarder {
    /// `local<TAB>dest`, for the ports file.
    pub(super) fn describe(&self) -> String {
        match self {
            Forwarder::Proxy(h) => format!("{}\t{}", h.bound.port(), h.dest),
            Forwarder::Ssh { local, dest, .. } => format!("{local}\t{dest}"),
        }
    }

    /// Where it forwards to: `ip:port` as reachable from the engine's host.
    pub(super) fn dest(&self) -> &str {
        match self {
            Forwarder::Proxy(h) => &h.dest,
            Forwarder::Ssh { dest, .. } => dest,
        }
    }

    // `dest` is "ip:port" as reachable from the engine's host.
    pub(super) fn proxy(local: u16, dest: String) -> std::io::Result<Self> {
        ProxyHandle::spawn(local, dest).map(Forwarder::Proxy)
    }

    pub(super) fn ssh_tunnel(local: u16, dest: &str, remote: &str) -> std::io::Result<Self> {
        let mut cmd = crate::ssh::tunnel(remote, local, dest);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            // With --verbose, let ssh's own chatter land in the daemon log.
            .stderr(if crate::output::is_verbose() {
                Stdio::inherit()
            } else {
                Stdio::null()
            });
        crate::output::trace(&cmd);
        cmd.spawn().map(|child| Forwarder::Ssh {
            child,
            local,
            dest: dest.to_string(),
        })
    }

    pub(super) fn is_alive(&mut self) -> bool {
        match self {
            Forwarder::Proxy(h) => !h.stop.load(Ordering::Relaxed),
            // A tunnel we can't query is treated as dead, so it is replaced.
            Forwarder::Ssh { child, .. } => child.try_wait().map(|s| s.is_none()).unwrap_or(false),
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        match self {
            Forwarder::Proxy(h) => h.stop.store(true, Ordering::Relaxed),
            Forwarder::Ssh { child, .. } => {
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
