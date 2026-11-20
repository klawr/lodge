//! The `ldg _forward` daemon: one per container, forwarding declared ports
//! from the start and any port that starts listening later.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::tunnel::Forwarder;
use super::{acquire_lock, lock_path, ports_path, truncate_log, write_ports};
use crate::port::PortForward;
use crate::runtime::{Container, ExecOpts, Runtime};
use crate::target::Target;

/// A line in the daemon's log (its stderr).
macro_rules! log {
    ($($arg:tt)*) => { $crate::output::scoped!("lodge forward", $($arg)*) };
}

/// How often the daemon looks at the container.
const POLL: Duration = Duration::from_secs(2);
/// How long a resolved address is trusted before it is looked up again, so a
/// restarted service (with a new address) is followed.
const REFRESH: Duration = Duration::from_secs(30);
/// How many polls in a row may fail to reach the engine before the daemon
/// gives up (about five minutes): a dropped connection is retried, a host that
/// is gone for good is not.
const UNREACHABLE_POLLS: u32 = 150;

/// Port-forwarding daemon. Forwards declared in `forwardPorts` are set up
/// eagerly, from startup — connections just fail until the service listens.
/// This also covers containers whose listeners can't be inspected (no shell for
/// `cat /proc/net/tcp`) and targets other than the primary container (compose
/// services). On top of that, the daemon polls `/proc/net/tcp` and
/// `/proc/net/tcp6` every 2 s and keeps one forwarder per listening port. For
/// local Docker, a built-in Rust TCP proxy is used; for `--remote`, an `ssh -L`
/// tunnel. Exits when the container stops, and keeps trying while the engine
/// can't be reached.
///
/// `ports` are the declared forwards as `spawn_daemon` encoded them on the
/// command line; `rt` is the engine at `target`.
pub fn run_daemon(rt: &dyn Runtime, target: &Target, container_id: &str, ports: &[String]) {
    let declared = decode_ports(ports);
    let (lock_file, ports_file) = match (lock_path(container_id), ports_path(container_id)) {
        (Ok(l), Ok(p)) => (l, p),
        (Err(e), _) | (_, Err(e)) => {
            log!("{e}; exiting");
            return;
        }
    };
    // Every `ldg up` starts a daemon; only one per container may run, or they
    // fight over the same local ports. The lock lives as long as this process.
    let _lock = match acquire_lock(&lock_file) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            log!("already running for this container; exiting");
            return;
        }
        Err(e) => {
            log!("could not take the forwarder lock ({e}); continuing without it");
            None
        }
    };
    if let Err(e) = truncate_log(container_id) {
        log!("could not start a new log ({e}); appending");
    }
    let container = rt.container(container_id);
    let mut forwards = Forwards::new(container.as_ref(), target, ports_file);
    let mut unreachable = 0;

    loop {
        match container.is_running() {
            Ok(true) => unreachable = 0,
            Ok(false) => {
                log!("container stopped, exiting");
                break;
            }
            Err(e) => {
                unreachable += 1;
                if unreachable == 1 {
                    log!("cannot reach the container engine ({e:#}); retrying");
                }
                if unreachable >= UNREACHABLE_POLLS {
                    log!("the container engine has been unreachable for too long, exiting");
                    break;
                }
                std::thread::sleep(POLL);
                continue;
            }
        }
        forwards.update(wanted(poll_listening_ports(container.as_ref()), &declared));
        std::thread::sleep(POLL);
    }
    forwards.shut_down();
}

fn decode_ports(ports: &[String]) -> Vec<PortForward> {
    ports
        .iter()
        .filter_map(|spec| {
            let decoded = PortForward::decode(spec);
            if decoded.is_none() {
                log!("ignoring malformed port {spec:?}");
            }
            decoded
        })
        .collect()
}

/// The forwards wanted now, keyed by local port: every listening port maps to
/// itself on the primary container; declared forwards are always wanted and
/// win a local-port collision.
fn wanted(listening: HashSet<u16>, declared: &[PortForward]) -> HashMap<u16, PortForward> {
    let mut wanted: HashMap<u16, PortForward> = listening
        .into_iter()
        .map(|p| (p, PortForward::same(p)))
        .collect();
    for f in declared {
        wanted.insert(f.local, f.clone());
    }
    wanted
}

/// The forwarders kept for one container, and the ports file that reports them.
struct Forwards<'a> {
    target: &'a Target,
    addresses: Addresses<'a>,
    active: HashMap<u16, Forwarder>,
    /// Local ports whose forwarder failed to start, so the retry every 2 s
    /// (likely failing the same way, e.g. host port occupied) doesn't spam the
    /// log.
    failed: HashSet<u16>,
    ports_file: PathBuf,
    /// What the ports file says; `None` until the first write, so the file
    /// exists (with the pid) at once.
    published: Option<Vec<String>>,
}

impl<'a> Forwards<'a> {
    fn new(container: &'a dyn Container, target: &'a Target, ports_file: PathBuf) -> Self {
        let lookup = move |host: Option<&str>| match host {
            None => container.ip_address(),
            Some(h) => container.resolve_peer(h),
        };
        Self {
            target,
            addresses: Addresses::new(Box::new(lookup), REFRESH),
            active: HashMap::new(),
            failed: HashSet::new(),
            ports_file,
            published: None,
        }
    }

    /// Brings the forwarders in line with `wanted`: drops the ones no longer
    /// wanted or whose process died, re-points the ones whose destination moved,
    /// and starts the missing ones.
    fn update(&mut self, wanted: HashMap<u16, PortForward>) {
        let mut dests = HashMap::new();
        for (&local, f) in &wanted {
            if let Some(ip) = self.addresses.get(f.host.as_deref()) {
                dests.insert(local, format!("{ip}:{}", f.port));
            }
        }
        self.active.retain(|&local, fwd| {
            let keep = fwd.is_alive() && dests.get(&local).is_some_and(|d| d == fwd.dest());
            if !keep {
                log!("stopped forwarding port {local}");
            }
            keep
        });
        for (local, dest) in dests {
            if !self.active.contains_key(&local) {
                self.start(local, dest);
            }
        }
        self.publish();
    }

    fn start(&mut self, local: u16, dest: String) {
        let fwd = if let Some(r) = self.target.ssh_host() {
            log!("port {local} — ssh -L {local}:{dest} {r}");
            Forwarder::ssh_tunnel(local, &dest, r)
        } else {
            log!("port {local} — proxy 127.0.0.1:{local} → {dest}");
            Forwarder::proxy(local, dest)
        };
        match fwd {
            Ok(fw) => {
                log!("forwarding port {local}");
                self.active.insert(local, fw);
                self.failed.remove(&local);
            }
            Err(e) => {
                if self.failed.insert(local) {
                    log!("could not forward port {local}: {e}");
                }
            }
        }
    }

    /// Rewrites the ports file when what is forwarded changed.
    fn publish(&mut self) {
        let mut now: Vec<String> = self.active.values().map(Forwarder::describe).collect();
        now.sort();
        if self.published.as_ref() != Some(&now) {
            if let Err(e) = write_ports(&self.ports_file, &now) {
                log!("could not record active ports: {e}");
            }
            self.published = Some(now);
        }
    }

    /// Stops every forwarder and removes the ports file.
    fn shut_down(self) {
        crate::tmpfile::discard_file(&self.ports_file);
    }
}

/// Looks up an address on the container network: the primary container's
/// (`None`) or a peer's by name.
type Lookup<'a> = Box<dyn Fn(Option<&str>) -> Option<String> + 'a>;

/// Addresses on the container network, as reachable from the engine's host:
/// the primary container's (`None`) and its peers' by name. Each is looked up
/// again after `refresh`, so a restarted container or service is followed; a
/// failed lookup keeps the last known address.
struct Addresses<'a> {
    lookup: Lookup<'a>,
    refresh: Duration,
    known: HashMap<Option<String>, (String, Instant)>,
    /// Names already reported as unresolvable (reported once).
    unresolved: HashSet<Option<String>>,
}

impl<'a> Addresses<'a> {
    fn new(lookup: Lookup<'a>, refresh: Duration) -> Self {
        Self {
            lookup,
            refresh,
            known: HashMap::new(),
            unresolved: HashSet::new(),
        }
    }

    fn get(&mut self, host: Option<&str>) -> Option<String> {
        let key = host.map(str::to_string);
        if let Some((ip, at)) = self.known.get(&key)
            && at.elapsed() < self.refresh
        {
            return Some(ip.clone());
        }
        let found = (self.lookup)(host);
        let name = host.unwrap_or("the container");
        match found {
            Some(ip) => {
                let previous = self.known.insert(key.clone(), (ip.clone(), Instant::now()));
                if previous.is_none_or(|(old, _)| old != ip) {
                    log!("{name} is at {ip}");
                }
                self.unresolved.remove(&key);
                Some(ip)
            }
            None => {
                if self.unresolved.insert(key.clone()) {
                    log!(
                        "cannot find {name} on the container network (not up yet?); will keep trying"
                    );
                }
                self.known.get(&key).map(|(ip, _)| ip.clone())
            }
        }
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
    fn addresses_follow_a_move_and_survive_a_failed_lookup() {
        // The container and its peer `db`, at an address the test moves.
        let ip = std::cell::RefCell::new(Some("10.0.0.2".to_string()));
        let lookup = || -> Lookup<'_> {
            Box::new(|host| match host {
                None | Some("db") => ip.borrow().clone(),
                Some(_) => None,
            })
        };
        let mut fresh = Addresses::new(lookup(), Duration::ZERO);
        let mut cached = Addresses::new(lookup(), Duration::from_secs(3600));
        assert_eq!(fresh.get(None).as_deref(), Some("10.0.0.2"));
        assert_eq!(cached.get(Some("db")).as_deref(), Some("10.0.0.2"));

        *ip.borrow_mut() = Some("10.0.0.9".into());
        assert_eq!(fresh.get(None).as_deref(), Some("10.0.0.9"), "moved");
        assert_eq!(
            cached.get(Some("db")).as_deref(),
            Some("10.0.0.2"),
            "not yet due"
        );

        *ip.borrow_mut() = None;
        assert_eq!(
            fresh.get(None).as_deref(),
            Some("10.0.0.9"),
            "last known kept"
        );
        assert_eq!(fresh.get(Some("nowhere")), None);
    }

    #[test]
    fn declared_forwards_win_a_local_port_collision() {
        let declared = [PortForward {
            local: 8080,
            host: Some("db".into()),
            port: 5432,
        }];
        let w = wanted(HashSet::from([8080, 3000]), &declared);
        assert_eq!(w[&3000], PortForward::same(3000));
        assert_eq!(w[&8080], declared[0]);
    }

    #[test]
    fn malformed_declared_ports_are_skipped() {
        assert_eq!(
            decode_ports(&["8080:80".into(), "nonsense".into()]),
            [PortForward {
                local: 8080,
                host: None,
                port: 80
            }]
        );
    }

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
