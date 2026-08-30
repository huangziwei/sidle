//! LAN search for sidle-server.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// The table [`netmask_from_route`] parses.
pub const ROUTE_TABLE: &str = "/proc/net/route";

/// Per-address TCP handshake budget in [`open_hosts`].
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// Addresses [`open_hosts`] probes at once.
const SWEEP_THREADS: usize = 32;

/// Stack size of each thread [`open_hosts`] spawns.
const SWEEP_STACK: usize = 64 * 1024;

/// Widest prefix [`candidates`] returns addresses for.
const MIN_PREFIX: u32 = 24;

/// This Kindle's IPv4 on the interface holding the default route.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-3 (RFC 5737), unrouted.
    sock.connect("203.0.113.1:80").ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) if !v4.ip().is_loopback() => Some(*v4.ip()),
        _ => None,
    }
}

/// An address from a [`ROUTE_TABLE`] hex field.
fn hex_ipv4(field: &str) -> Option<Ipv4Addr> {
    if field.len() != 8 {
        return None;
    }
    let v = u32::from_str_radix(field, 16).ok()?;
    let [a, b, c, d] = v.to_be_bytes();
    Some(Ipv4Addr::new(d, c, b, a))
}

/// The netmask of the directly-connected route holding `ip`, from [`ROUTE_TABLE`]
/// text.
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

/// Every address on `ip`'s subnet, ascending, minus `ip` and the network and
/// broadcast addresses.
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
/// [`SWEEP_THREADS`] threads share one cursor over `hosts`.
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
            if spawned.is_err() {
                break;
            }
        }
    });

    let mut hits = found.into_inner().unwrap();
    hits.sort_unstable();
    hits.into_iter().map(|i| hosts[i]).collect()
}

/// The first address on this Kindle's subnet that `verify` accepts, with one
/// `log` line per stage.
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

    /// A [`ROUTE_TABLE`] holding a default route and a connected /24.
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

    /// The default route's zero mask matches every address.
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

    /// A /16 mask carries 65k addresses; [`MIN_PREFIX`] caps the sweep at the
    /// /24 holding `ip`.
    #[test]
    fn a_wide_mask_is_narrowed_to_a_single_subnet() {
        let hosts = candidates(Ipv4Addr::new(10, 4, 7, 9), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(hosts.len(), 253);
        assert!(hosts.contains(&Ipv4Addr::new(10, 4, 7, 1)));
        assert!(!hosts.contains(&Ipv4Addr::new(10, 4, 8, 1)));
    }

    /// A /28 is 13 addresses.
    #[test]
    fn a_narrow_mask_is_kept() {
        let hosts = candidates(
            Ipv4Addr::new(192, 168, 0, 33),
            Ipv4Addr::new(255, 255, 255, 240),
        );
        // .32 and .47 bound this /28, and .33 is `ip`.
        assert_eq!(hosts.len(), 13);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 0, 34));
        assert_eq!(hosts[12], Ipv4Addr::new(192, 168, 0, 46));
    }

    /// [`open_hosts`] against a bound `TcpListener`: the thread pool, the
    /// cursor, and the result order.
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
