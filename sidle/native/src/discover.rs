//! Find sidle-server on the LAN when `HOST=` no longer answers.
//!
//! `etc/server.conf` carries the Mac's address as of the last cable install,
//! and that address moves: DHCP hands out a new one, the Mac joins a different
//! network. The picker then dials a host that is gone, and the file naming the
//! new one is the single thing a Wi-Fi pull is not allowed to deliver
//! (`apps::policy::PER_INSTALL` on the desktop) — so without a search here, a
//! moved server can only be found again over a cable.
//!
//! The search is a TCP sweep of this Kindle's own subnet on the server port,
//! and every hit then has to answer a real request through the CA-pinned agent.
//! That pin is what makes the sweep safe to trust: only the machine holding the
//! key to `etc/ca.pem`'s CA can complete the handshake, so "listens on 8731 and
//! answers" identifies our server and nothing else. The token never leaves the
//! device until the address it is about to be sent to has proved itself.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// The kernel's routing table, in the format [`netmask_from_route`] parses.
pub const ROUTE_TABLE: &str = "/proc/net/route";

/// How long one candidate gets to complete a TCP handshake. A LAN peer answers
/// in single-digit milliseconds; this is sized for a sleepy radio, not a slow
/// host.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// Candidates probed at once. The sweep is latency-bound, not CPU-bound, so
/// this is set by how long the whole search may take: a /24 in ~8 rounds.
const SWEEP_THREADS: usize = 32;

/// Stack per sweep thread. A `connect` and an `Ipv4Addr` need almost nothing,
/// and the default 2 MiB reservation times [`SWEEP_THREADS`] is real address
/// space on a 512 MB device.
const SWEEP_STACK: usize = 64 * 1024;

/// Widest subnet swept, as a prefix length. A home LAN is a /24; a /16 route
/// would be 65k probes for a machine that is almost certainly in this Kindle's
/// own /24 anyway, so a wider mask is narrowed to this before sweeping.
const MIN_PREFIX: u32 = 24;

/// This Kindle's IPv4 on the interface holding the default route.
///
/// The no-packet UDP trick, the same one the desktop uses to decide what to
/// write into `HOST=`: `connect` on a UDP socket only fixes the route, so the
/// kernel picks the outbound interface and names its address, and nothing is
/// sent. `None` when no interface routes anywhere — a Kindle with the radio
/// off, which no amount of sweeping would help.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-3 (RFC 5737): guaranteed unrouted, and tells an observer
    // nothing about this device.
    sock.connect("203.0.113.1:80").ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) if !v4.ip().is_loopback() => Some(*v4.ip()),
        _ => None,
    }
}

/// An address from `/proc/net/route`'s hex form.
///
/// The kernel prints these u32s in host byte order, so the leading hex pair is
/// the *last* octet on every target this runs on (armv7 and the hosts the tests
/// run on are all little-endian).
fn hex_ipv4(field: &str) -> Option<Ipv4Addr> {
    if field.len() != 8 {
        return None;
    }
    let v = u32::from_str_radix(field, 16).ok()?;
    let [a, b, c, d] = v.to_be_bytes();
    Some(Ipv4Addr::new(d, c, b, a))
}

/// The netmask of the directly-connected route `ip` sits in, read from the text
/// of `/proc/net/route`.
///
/// A gateway route (`Gateway` non-zero) names the wider internet and says
/// nothing about who is on this wire, so only routes with no gateway are
/// considered, and among those the one whose network `ip` actually falls in.
pub fn netmask_from_route(table: &str, ip: Ipv4Addr) -> Option<Ipv4Addr> {
    let want = u32::from(ip);
    for line in table.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (_iface, dest, gateway) = (f.next()?, f.next()?, f.next()?);
        let mask = f.nth(4)?;
        if gateway != "00000000" {
            continue;
        }
        let (Some(dest), Some(mask)) = (hex_ipv4(dest), hex_ipv4(mask)) else {
            continue;
        };
        let mask_bits = u32::from(mask);
        if mask_bits != 0 && want & mask_bits == u32::from(dest) {
            return Some(mask);
        }
    }
    None
}

/// Every address to probe on `ip`'s subnet, ascending, without `ip` itself and
/// without the network and broadcast addresses.
///
/// A mask wider than [`MIN_PREFIX`] is narrowed to it: the sweep is bounded by
/// what can finish in seconds, not by what the route table permits.
pub fn candidates(ip: Ipv4Addr, mask: Ipv4Addr) -> Vec<Ipv4Addr> {
    let narrowed = u32::from(mask) | !(u32::MAX >> MIN_PREFIX);
    let host = u32::from(ip);
    let network = host & narrowed;
    let broadcast = network | !narrowed;
    (network + 1..broadcast)
        .filter(|&a| a != host)
        .map(Ipv4Addr::from)
        .collect()
}

/// Which of `hosts` accept a TCP connection on `port`, in the order given.
///
/// Concurrent because the cost is [`CONNECT_TIMEOUT`] per dead address and a
/// subnet is mostly dead addresses. An open port is a candidate the caller then
/// puts to a TLS request, never an answer on its own.
pub fn open_hosts(hosts: &[Ipv4Addr], port: u16) -> Vec<Ipv4Addr> {
    let next = AtomicUsize::new(0);
    let found: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    thread::scope(|scope| {
        for _ in 0..SWEEP_THREADS.min(hosts.len()) {
            let (next, found) = (&next, &found);
            let spawned =
                thread::Builder::new()
                    .stack_size(SWEEP_STACK)
                    .spawn_scoped(scope, move || {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&host) = hosts.get(i) else { return };
                            let addr = SocketAddr::from((host, port));
                            if TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok() {
                                found.lock().unwrap().push(i);
                            }
                        }
                    });
            // A thread that will not start costs the search its share of the
            // parallelism and nothing else — the ones already running drain the
            // whole list between them.
            if spawned.is_err() {
                break;
            }
        }
    });

    let mut hits = found.into_inner().unwrap();
    hits.sort_unstable();
    hits.into_iter().map(|i| hosts[i]).collect()
}

/// Sweep this Kindle's subnet for a host `verify` accepts as sidle-server.
///
/// `verify` is the CA-pinned request that settles identity; `log` receives one
/// line per stage, which is what a user reads back when a search comes up
/// empty. `None` when the radio is down, the subnet holds nothing listening, or
/// nothing listening is ours.
pub fn find_server(
    port: u16,
    verify: impl Fn(Ipv4Addr) -> bool,
    log: impl Fn(&str),
) -> Option<Ipv4Addr> {
    let ip = local_ipv4()?;
    let mask = std::fs::read_to_string(ROUTE_TABLE)
        .ok()
        .and_then(|table| netmask_from_route(&table, ip))
        .unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
    let hosts = candidates(ip, mask);
    log(&format!(
        "search: this Kindle is {ip}/{mask}, sweeping {} address(es) on port {port}",
        hosts.len()
    ));

    let open = open_hosts(&hosts, port);
    log(&format!("search: {} host(s) listening", open.len()));
    for host in open {
        if verify(host) {
            log(&format!("search: {host} is sidle-server"));
            return Some(host);
        }
        log(&format!("search: {host} listens but is not sidle-server"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Kindle's table: the default route through the gateway, then the
    /// directly-connected /24.
    const TABLE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t00000000\t0100A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0
wlan0\t0000A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
";

    #[test]
    fn hex_fields_are_little_endian() {
        assert_eq!(hex_ipv4("0100A8C0"), Some(Ipv4Addr::new(192, 168, 0, 1)));
        assert_eq!(hex_ipv4("00FFFFFF"), Some(Ipv4Addr::new(255, 255, 255, 0)));
        assert_eq!(hex_ipv4("00000000"), Some(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(hex_ipv4("nonsense"), None);
        assert_eq!(hex_ipv4("C0A8"), None);
    }

    #[test]
    fn the_connected_route_supplies_the_mask() {
        assert_eq!(
            netmask_from_route(TABLE, Ipv4Addr::new(192, 168, 0, 33)),
            Some(Ipv4Addr::new(255, 255, 255, 0))
        );
    }

    /// The default route matches every address through a zero mask. Taking it
    /// would sweep the whole internet.
    #[test]
    fn the_default_route_is_never_the_mask() {
        assert_eq!(netmask_from_route(TABLE, Ipv4Addr::new(10, 0, 0, 5)), None);
    }

    #[test]
    fn a_subnet_yields_its_hosts_without_self_network_or_broadcast() {
        let hosts = candidates(
            Ipv4Addr::new(192, 168, 0, 33),
            Ipv4Addr::new(255, 255, 255, 0),
        );
        assert_eq!(hosts.len(), 253);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 0, 1));
        assert_eq!(hosts[hosts.len() - 1], Ipv4Addr::new(192, 168, 0, 254));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 0, 33)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 0, 0)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 0, 255)));
    }

    /// A /16 route is 65k probes. The server is on this Kindle's own /24 in
    /// every case worth sweeping for.
    #[test]
    fn a_wide_mask_is_narrowed_to_a_single_subnet() {
        let hosts = candidates(Ipv4Addr::new(10, 4, 7, 9), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(hosts.len(), 253);
        assert!(hosts.contains(&Ipv4Addr::new(10, 4, 7, 1)));
        assert!(!hosts.contains(&Ipv4Addr::new(10, 4, 8, 1)));
    }

    /// A narrower mask is kept — a /28 is 13 probes, not 253.
    #[test]
    fn a_narrow_mask_is_kept() {
        let hosts = candidates(
            Ipv4Addr::new(192, 168, 0, 33),
            Ipv4Addr::new(255, 255, 255, 240),
        );
        // .32/.47 are the network and broadcast of this /28, and .33 is us.
        assert_eq!(hosts.len(), 13);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 0, 34));
        assert_eq!(hosts[12], Ipv4Addr::new(192, 168, 0, 46));
    }

    /// The sweep runs against a real listener, so the thread pool, the cursor
    /// and the result ordering are exercised rather than described.
    #[test]
    fn a_sweep_reports_the_listener_it_finds() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hosts = [
            Ipv4Addr::new(127, 0, 0, 2),
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(127, 0, 0, 3),
        ];
        assert_eq!(open_hosts(&hosts, port), vec![Ipv4Addr::new(127, 0, 0, 1)]);
    }

    #[test]
    fn an_empty_candidate_list_sweeps_nothing() {
        assert!(open_hosts(&[], 8731).is_empty());
    }
}
