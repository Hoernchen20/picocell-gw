//! Build-time configuration (`fw/.cargo/config.toml`, `[env]`). Stand-in until the
//! encrypted config store and USB provisioning exist (sketch.md section 7).

use core::net::Ipv4Addr;
use core::str::FromStr;

use embassy_net::{Ipv4Address, Ipv4Cidr, StaticConfigV4};

pub const CA_DER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ca.der"));
pub const MQTT_HOST: &str = env!("PICOCELL_MQTT_HOST");
pub const MQTT_PASSWORD: &str = env!("PICOCELL_MQTT_PASSWORD");
pub const TOPIC_PREFIX: &str = env!("PICOCELL_TOPIC_PREFIX");
/// Unix time of the build; used as the clock until SNTP exists.
pub const BUILD_TIME: &str = env!("PICOCELL_BUILD_TIME");

const NET: &str = env!("PICOCELL_NET");
const NET_GATEWAY: &str = env!("PICOCELL_NET_GATEWAY");
const MQTT_ADDR: &str = env!("PICOCELL_MQTT_ADDR");
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

/// Broker address and port; `None` if invalid.
pub fn broker() -> Option<(Ipv4Address, u16)> {
    Some((Ipv4Addr::from_str(MQTT_ADDR).ok()?, u16::from_str(MQTT_PORT).ok()?))
}

pub fn build_time() -> u64 {
    u64::from_str(BUILD_TIME).unwrap_or(0)
}
