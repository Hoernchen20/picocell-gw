//! Build-time configuration (`fw/.cargo/config.toml`, `[env]`). Stand-in until the
//! encrypted config store and USB provisioning exist (sketch.md section 7).

use core::net::Ipv4Addr;
use core::str::FromStr;

use embassy_net::{Ipv4Cidr, StaticConfigV4};
use heapless::Vec;

pub const CA_DER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ca.der"));
pub const MQTT_HOST: &str = env!("PICOCELL_MQTT_HOST");
pub const MQTT_PASSWORD: &str = env!("PICOCELL_MQTT_PASSWORD");
pub const TOPIC_PREFIX: &str = env!("PICOCELL_TOPIC_PREFIX");
/// Unix time of the build; SNTP results before it are rejected.
pub const BUILD_TIME: &str = env!("PICOCELL_BUILD_TIME");

const NET: &str = env!("PICOCELL_NET");
const NET_GATEWAY: &str = env!("PICOCELL_NET_GATEWAY");
const HOSTS: &str = env!("PICOCELL_HOSTS");
const DNS: &str = env!("PICOCELL_DNS");
const NTP: &str = env!("PICOCELL_NTP");
const MQTT_PORT: &str = env!("PICOCELL_MQTT_PORT");

pub enum Net {
    Dhcp,
    Static(StaticConfigV4),
}

/// Parses `PICOCELL_NET` / `PICOCELL_NET_GATEWAY`; `None` if invalid.
pub fn net() -> Option<Net> {
    if NET == "dhcp" {
        return Some(Net::Dhcp);
    }
    let (addr, prefix) = NET.split_once('/')?;
    let addr = Ipv4Addr::from_str(addr).ok()?;
    let prefix = u8::from_str(prefix).ok().filter(|p| *p <= 32)?;
    let gateway = match NET_GATEWAY {
        "" => None,
        g => Some(Ipv4Addr::from_str(g).ok()?),
    };
    Some(Net::Static(StaticConfigV4 {
        address: Ipv4Cidr::new(addr, prefix),
        gateway,
        dns_servers: Default::default(),
    }))
}

/// Comma-separated list, blanks around entries ignored.
fn list(s: &'static str) -> impl Iterator<Item = &'static str> {
    s.split(',').map(str::trim).filter(|e| !e.is_empty())
}

/// Hosts table (`PICOCELL_HOSTS`, `name=address,...`); `None` if invalid.
pub fn hosts() -> Option<Vec<(&'static str, Ipv4Addr), 4>> {
    let mut v = Vec::new();
    for e in list(HOSTS) {
        let (name, addr) = e.split_once('=')?;
        let name = name.trim();
        if name.is_empty() || name.len() > 63 {
            return None;
        }
        v.push((name, Ipv4Addr::from_str(addr.trim()).ok()?)).ok()?;
    }
    Some(v)
}

/// DNS servers (`PICOCELL_DNS`), tried before any from DHCP; `None` if invalid.
pub fn dns_servers() -> Option<Vec<Ipv4Addr, 3>> {
    let mut v = Vec::new();
    for e in list(DNS) {
        v.push(Ipv4Addr::from_str(e).ok()?).ok()?;
    }
    Some(v)
}

/// SNTP servers (`PICOCELL_NTP`), IP addresses or host names; `None` if invalid.
pub fn ntp_servers() -> Option<Vec<&'static str, 3>> {
    let mut v = Vec::new();
    for e in list(NTP) {
        v.push(e).ok()?;
    }
    (!v.is_empty()).then_some(v)
}

pub fn mqtt_port() -> Option<u16> {
    u16::from_str(MQTT_PORT).ok()
}

pub fn build_time() -> u64 {
    u64::from_str(BUILD_TIME).unwrap_or(0)
}
