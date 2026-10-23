//! Per-board identifiers derived from the RP2350 unique chip ID (sketch.md section 3.4).

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
    /// Placeholder gateway ID (EUI-64) until the SX1302 EUI can be read: the chip ID.
    pub gateway_id: heapless::String<16>,
}

impl Ids {
    pub fn read() -> Self {
        let chip_id = embassy_rp::otp::get_chipid().unwrap_or(0);
        let h = fnv1a(b"mac", &chip_id.to_be_bytes()).to_be_bytes();
        let mut mac = [h[0], h[1], h[2], h[3], h[4], h[5]];
        mac[0] = (mac[0] & !0x01) | 0x02;
        let mut gateway_id = heapless::String::new();
        let _ = write!(gateway_id, "{chip_id:016x}");
        Self { mac, gateway_id }
    }
}
