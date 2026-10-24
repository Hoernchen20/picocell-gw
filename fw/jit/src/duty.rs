//! EU868 duty cycle (ETSI EN 300 220-2, ERC Recommendation 70-03 annex 1): the airtime
//! in each sub-band within any one hour must stay below the band's share of that hour.
//!
//! Sliding window in one-minute buckets. A frame is charged to the minute of its start,
//! and the check adds up every bucket that may still lie in the window, including
//! frames queued for the next minute. That counts up to 62 minutes instead of 60, on
//! the safe side. The buckets live in RAM, so a reboot starts with an empty window.

use core::ops::RangeInclusive;

/// One sub-band.
pub struct Band {
    pub name: &'static str,
    /// Lowest and highest frequency a frame may occupy, edges included.
    pub hz: RangeInclusive<u32>,
    /// ChirpStack's `power` is EIRP; 25 mW ERP ≈ 16 dBm EIRP.
    pub max_eirp_dbm: i32,
    /// Duty cycle in 1/1000.
    pub permille: u32,
}

impl Band {
    /// Allowed airtime per window.
    pub const fn limit_us(&self) -> u64 {
        WINDOW_US / 1000 * self.permille as u64
    }
}

pub const WINDOW_US: u64 = 3_600_000_000;

/// The EU868 sub-bands without the alarm and social-alarm bands in between, which are
/// not for LoRaWAN. 27 dBm in 869.4–869.65 MHz is the WM1302's maximum as well.
pub const EU868: [Band; 6] = [
    Band { name: "863.0-865.0", hz: 863_000_000..=865_000_000, max_eirp_dbm: 16, permille: 1 },
    Band { name: "865.0-868.0", hz: 865_000_000..=868_000_000, max_eirp_dbm: 16, permille: 10 },
    Band { name: "868.0-868.6", hz: 868_000_000..=868_600_000, max_eirp_dbm: 16, permille: 10 },
    Band { name: "868.7-869.2", hz: 868_700_000..=869_200_000, max_eirp_dbm: 16, permille: 1 },
    Band { name: "869.4-869.65", hz: 869_400_000..=869_650_000, max_eirp_dbm: 27, permille: 100 },
    Band { name: "869.7-870.0", hz: 869_700_000..=870_000_000, max_eirp_dbm: 16, permille: 10 },
];

/// Index of the band that holds a whole frame on `frequency` with `bandwidth`.
pub fn band(bands: &[Band], frequency: u32, bandwidth: u32) -> Option<usize> {
    let lo = frequency.checked_sub(bandwidth / 2)?;
    let hi = frequency.checked_add(bandwidth / 2)?;
    bands.iter().position(|b| b.hz.contains(&lo) && b.hz.contains(&hi))
}

const BUCKET_US: u64 = 60_000_000;
/// The 60 minutes back, the current one and the next: frames start at most
/// `MAX_ADVANCE_US` (one minute) ahead.
const BUCKETS: usize = 62;

#[derive(Clone, Copy)]
struct Bucket {
    minute: u32,
    airtime_us: u32,
}

/// Airtime per band. Times are monotonic µs (not the wrapping `count_us`).
pub struct Tracker<const B: usize> {
    bands: [[Bucket; BUCKETS]; B],
}

impl<const B: usize> Default for Tracker<B> {
    fn default() -> Self {
        Self::new()
    }
}

fn minute(t_us: u64) -> u32 {
    (t_us / BUCKET_US) as u32
}

impl<const B: usize> Tracker<B> {
    pub const fn new() -> Self {
        Self { bands: [[Bucket { minute: 0, airtime_us: 0 }; BUCKETS]; B] }
    }

    /// Airtime counted against `band` at `now_us`.
    pub fn used_us(&self, band: usize, now_us: u64) -> u64 {
        let now = minute(now_us);
        self.bands[band]
            .iter()
            .filter(|b| b.minute.saturating_add(60) >= now)
            .map(|b| u64::from(b.airtime_us))
            .sum()
    }

    /// Whether `airtime_us` more fits into the window.
    pub fn allows(&self, bands: &[Band], band: usize, now_us: u64, airtime_us: u32) -> bool {
        self.used_us(band, now_us) + u64::from(airtime_us) <= bands[band].limit_us()
    }

    /// Records a frame starting at `start_us`, at most one minute after now.
    pub fn charge(&mut self, band: usize, start_us: u64, airtime_us: u32) {
        let m = minute(start_us);
        let b = &mut self.bands[band][m as usize % BUCKETS];
        if b.minute != m {
            // 62 minutes old: out of the window.
            *b = Bucket { minute: m, airtime_us: 0 };
        }
        b.airtime_us = b.airtime_us.saturating_add(airtime_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000_000;

    #[test]
    fn bands() {
        assert_eq!(band(&EU868, 868_100_000, 125_000), Some(2));
        assert_eq!(band(&EU868, 867_900_000, 125_000), Some(1));
        assert_eq!(band(&EU868, 869_525_000, 125_000), Some(4));
        // Straddles the 868.0 MHz edge.
        assert_eq!(band(&EU868, 868_000_000, 125_000), None);
        assert_eq!(band(&EU868, 868_300_000, 250_000), Some(2));
        assert_eq!(band(&EU868, 868_650_000, 125_000), None);
        assert_eq!(band(&EU868, 862_000_000, 125_000), None);
        assert_eq!(EU868[0].limit_us(), 3_600_000);
        assert_eq!(EU868[4].limit_us(), 360_000_000);
    }

    #[test]
    fn window() {
        let mut t: Tracker<6> = Tracker::new();
        let now = 100 * MIN;
        // 0.1 %: 3.6 s per hour, three 1.2 s frames.
        for i in 0..3 {
            assert!(t.allows(&EU868, 0, now, 1_200_000));
            t.charge(0, now + i * 10_000_000, 1_200_000);
        }
        assert!(!t.allows(&EU868, 0, now, 1));
        // Other bands are not affected.
        assert!(t.allows(&EU868, 3, now, 3_600_000));
        // Still counted 60 minutes later (and a bit more), free after 61.
        assert!(!t.allows(&EU868, 0, now + 60 * MIN, 1));
        assert_eq!(t.used_us(0, now + 61 * MIN - 1), 3_600_000);
        assert_eq!(t.used_us(0, now + 61 * MIN), 0);
        assert!(t.allows(&EU868, 0, now + 61 * MIN, 3_600_000));
    }

    #[test]
    fn next_minute_and_reuse() {
        let mut t: Tracker<1> = Tracker::new();
        let now = 10 * MIN + 59_000_000;
        // Queued for the next minute: counts now already.
        t.charge(0, now + 30_000_000, 1_000);
        assert_eq!(t.used_us(0, now), 1_000);
        // The same slot 62 minutes later starts from zero.
        let later = 11 * MIN + 62 * MIN;
        t.charge(0, later, 500);
        assert_eq!(t.used_us(0, later), 500);
    }
}
