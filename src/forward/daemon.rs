//! The `ldg _forward` daemon: one per container, forwarding declared ports
//! from the start and any port that starts listening later.

use crate::output::scoped;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::tunnel::Forwarder;
use super::{PortForward, acquire_lock, lock_path, ports_path, write_ports};
use crate::runtime::{self, Container, ExecOpts};
use crate::target::Target;

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
    // What `ldg ports` reads: one line per active forward, rewritten on change.
    let ports_file = ports_path(container_id);
    // `None` until the first write, so the file exists (with the pid) at once.
    let mut published: Option<Vec<String>> = None;

    loop {
        if !container.is_running() {
            scoped!("lodge forward", "container stopped, exiting");
            crate::tmpfile::discard_file(&ports_file);
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

        let mut now: Vec<String> = active.values().map(Forwarder::describe).collect();
        now.sort();
        if published.as_ref() != Some(&now) {
            if let Err(e) = write_ports(&ports_file, &now) {
                scoped!("lodge forward", "could not record active ports: {e}");
            }
            published = Some(now);
        }

        std::thread::sleep(Duration::from_secs(2));
    }
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
}
