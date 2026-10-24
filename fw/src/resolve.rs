//! Name resolution like `hosts: files dns` (sketch.md section 3.4): IP literal, hosts
//! table, DNS (static servers, then DHCP ones), last known address.

use core::cell::RefCell;
use core::fmt;
use core::net::Ipv4Addr;
use core::str::FromStr;

use critical_section::Mutex;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::{Duration, Instant, with_timeout};
use heapless::{String, Vec};
use log::warn;
use wire::dns;

use crate::{config, tls};

const TIMEOUT: Duration = Duration::from_secs(2);
const TRIES: usize = 2;

#[derive(Debug)]
pub enum Error {
    /// No hosts entry and no DNS server configured.
    NoServer,
    /// DNS failed and no earlier answer is known; the last server's error.
    Dns(dns::Error),
    /// No DNS server answered.
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoServer => f.write_str("no hosts entry and no DNS server"),
            Error::Dns(e) => write!(f, "{e}"),
            Error::Timeout => f.write_str("no DNS answer"),
        }
    }
}

struct Entry {
    name: String<64>,
    addr: Ipv4Address,
    expires: Instant,
}

/// DNS answers; kept past their TTL as "last known address".
static CACHE: Mutex<RefCell<Vec<Entry, 4>>> = Mutex::new(RefCell::new(Vec::new()));

/// Host names are compared case-insensitively, ignoring a trailing dot.
fn same_name(a: &str, b: &str) -> bool {
    let a = a.strip_suffix('.').unwrap_or(a);
    let b = b.strip_suffix('.').unwrap_or(b);
    a.eq_ignore_ascii_case(b)
}

/// The cached address for `name` and whether it is still within its TTL.
fn cached(name: &str) -> Option<(Ipv4Address, bool)> {
    critical_section::with(|cs| {
        let cache = CACHE.borrow_ref(cs);
        let e = cache.iter().find(|e| same_name(&e.name, name))?;
        Some((e.addr, Instant::now() < e.expires))
    })
}

fn remember(name: &str, addr: Ipv4Address, ttl: u32) {
    let Ok(key) = String::try_from(name) else {
        return; // Longer than 64 characters: not cached.
    };
    let entry = Entry { name: key, addr, expires: Instant::now() + Duration::from_secs(ttl.into()) };
    critical_section::with(|cs| {
        let mut cache = CACHE.borrow_ref_mut(cs);
        if let Some(i) = cache.iter().position(|e| same_name(&e.name, name)) {
            cache[i] = entry;
        } else if let Err(entry) = cache.push(entry) {
            // Full: replace the entry that expires first.
            let i = (0..cache.len()).min_by_key(|i| cache[*i].expires).unwrap();
            cache[i] = entry;
        }
    })
}

pub async fn resolve(stack: Stack<'_>, name: &str) -> Result<Ipv4Address, Error> {
    if let Ok(addr) = Ipv4Addr::from_str(name) {
        return Ok(addr);
    }
    let hosts = config::get().hosts();
    if let Some((_, addr)) = hosts.iter().find(|(n, _)| same_name(n, name)) {
        return Ok(*addr);
    }
    let last = cached(name);
    if let Some((addr, true)) = last {
        return Ok(addr);
    }

    let mut servers = config::get().dns_servers();
    if let Some(c) = stack.config_v4() {
        for s in c.dns_servers {
            if !servers.contains(&s) {
                let _ = servers.push(s);
            }
        }
    }
    if servers.is_empty() {
        return last.map(|(addr, _)| addr).ok_or(Error::NoServer);
    }

    let mut err = Error::Timeout;
    for server in servers {
        for _ in 0..TRIES {
            match query(stack, server, name).await {
                Ok(a) => {
                    let addr = Ipv4Address::from(a.addr);
                    remember(name, addr, a.ttl);
                    return Ok(addr);
                }
                // Not worth a retry.
                Err(Error::Dns(e @ dns::Error::InvalidName)) => return Err(Error::Dns(e)),
                Err(e @ Error::Dns(dns::Error::NotFound)) => {
                    err = e;
                    break;
                }
                Err(e) => err = e,
            }
        }
    }
    match last {
        Some((addr, _)) => {
            warn!("dns: {name}: {err}, using last known address {addr}");
            Ok(addr)
        }
        None => Err(err),
    }
}

async fn query(stack: Stack<'_>, server: Ipv4Address, name: &str) -> Result<dns::Answer, Error> {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 512];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buf = [0u8; 300];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    // Random ID and source port make forged answers harder to place.
    let mut r = [0u8; 4];
    tls::random_bytes(&mut r);
    let id = u16::from_be_bytes([r[0], r[1]]);
    let port = 49152 + u16::from_be_bytes([r[2], r[3]]) % 16384;
    socket.bind(port).map_err(|_| Error::Timeout)?;

    let mut msg = [0u8; 512];
    let n = dns::build_query(id, name, &mut msg).map_err(Error::Dns)?;
    socket.send_to(&msg[..n], (server, dns::PORT)).await.map_err(|_| Error::Timeout)?;
    let from = IpAddress::from(server);
    with_timeout(TIMEOUT, async {
        loop {
            let Ok((n, meta)) = socket.recv_from(&mut msg).await else {
                continue; // Larger than 512 B: not an answer to us (no EDNS).
            };
            if meta.endpoint.addr != from || meta.endpoint.port != dns::PORT {
                continue;
            }
            match dns::parse_response(id, name, &msg[..n]) {
                Err(dns::Error::Mismatch) => continue,
                r => return r.map_err(Error::Dns),
            }
        }
    })
    .await
    .map_err(|_| Error::Timeout)?
}
