//! Per-board identifiers (sketch.md section 3.4): the MAC derived from the RP2350 unique
//! chip ID, the gateway ID from the SX1302 EUI.

use core::fmt::Write;

/// FNV-1a 64, with a tag so the MAC and other derived values differ.
fn fnv1a(tag: &[u8], data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in tag.iter().chain(data) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub struct Ids {
    /// Locally administered unicast MAC (`x2:xx:xx:xx:xx:xx`).
    pub mac: [u8; 6],
    /// The SX1302 EUI in hex, or the chip ID if the concentrator did not start.
    pub gateway_id: heapless::String<16>,
    pub from_eui: bool,
}

impl Ids {
    /// `eui` from the first concentrator start.
    pub fn read(eui: Option<u64>) -> Self {
        let chip_id = embassy_rp::otp::get_chipid().unwrap_or(0);
        let h = fnv1a(b"mac", &chip_id.to_be_bytes()).to_be_bytes();
        let mut mac = [h[0], h[1], h[2], h[3], h[4], h[5]];
        mac[0] = (mac[0] & !0x01) | 0x02;
        let mut gateway_id = heapless::String::new();
        let _ = write!(gateway_id, "{:016x}", eui.unwrap_or(chip_id));
        Self { mac, gateway_id, from_eui: eui.is_some() }
    }
}
