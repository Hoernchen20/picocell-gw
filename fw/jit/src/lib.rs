//! Just-in-time downlink queue (sketch.md section 3.3), free of I/O so it can be tested
//! on the host. Modelled on the Lora-net packet forwarder's `jitqueue.c`.
//!
//! Times are SX1302 `count_us` values: a free-running 1 MHz `u32` counter that wraps
//! every ~71.6 min. They are only ever compared relative to "now", so the wrap does not
//! matter as long as everything lies within ±35 min of it.

#![no_std]

pub mod duty;

use heapless::Vec;

/// From handing a frame to the radio to the start of its preamble.
pub const TX_START_DELAY_US: u32 = 1_500;
/// Guard time around each frame.
pub const TX_MARGIN_US: u32 = 1_000;
/// How long before its start the radio task takes a frame from the queue.
pub const HANDOVER_US: u32 = 30_000;
/// Frames that start sooner than this when queued are too late.
pub const MIN_LEAD_US: u32 = 40_000 + TX_START_DELAY_US + TX_MARGIN_US;
/// Frames further ahead are too early. Class A RX2 is at most 16 s after the uplink,
/// and there are no Class B beacons.
pub const MAX_ADVANCE_US: u32 = 60_000_000;

/// Occupied before the start and after the end of each frame.
const PRE: i64 = (TX_START_DELAY_US + TX_MARGIN_US) as i64;
const POST: i64 = TX_MARGIN_US as i64;

#[derive(Debug, PartialEq)]
pub enum Error {
    TooLate,
    TooEarly,
    Collision,
    Full,
}

/// When to send.
#[derive(Clone, Copy)]
pub enum At {
    /// At this `count_us` (Class A: uplink counter + RX delay).
    Count(u32),
    /// In the first free slot (Class C).
    Asap,
}

/// Signed distance from `now` to `t` in µs, across the counter wrap.
pub fn offset(t: u32, now: u32) -> i32 {
    t.wrapping_sub(now) as i32
}

struct Entry<T> {
    start: u32,
    airtime_us: u32,
    item: T,
}

impl<T> Entry<T> {
    /// Occupied interval relative to `now`.
    fn span(&self, now: u32) -> (i64, i64) {
        let s = i64::from(offset(self.start, now));
        (s - PRE, s + i64::from(self.airtime_us) + POST)
    }
}

/// Up to `N` frames, sorted by start time and not overlapping.
pub struct Queue<T, const N: usize> {
    entries: Vec<Entry<T>, N>,
    /// Handed to the radio and not done yet: still blocks its airtime.
    on_air: Option<Entry<()>>,
}

impl<T, const N: usize> Default for Queue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Queue<T, N> {
    pub const fn new() -> Self {
        Self { entries: Vec::new(), on_air: None }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Queues `item` and returns its start time.
    pub fn enqueue(&mut self, now: u32, at: At, airtime_us: u32, item: T) -> Result<u32, Error> {
        if self.entries.is_full() {
            return Err(Error::Full);
        }
        let air = i64::from(airtime_us);
        let collides = |(b0, b1): (i64, i64), s: i64| s - PRE < b1 && b0 < s + air + POST;
        // The frame on air comes before all queued ones.
        let spans = || self.on_air.iter().map(|e| e.span(now)).chain(self.entries.iter().map(|e| e.span(now)));
        let start = match at {
            At::Count(t) => {
                let s = i64::from(offset(t, now));
                if s < i64::from(MIN_LEAD_US) {
                    return Err(Error::TooLate);
                }
                if s > i64::from(MAX_ADVANCE_US) {
                    return Err(Error::TooEarly);
                }
                if spans().any(|b| collides(b, s)) {
                    return Err(Error::Collision);
                }
                s
            }
            At::Asap => {
                // Sorted and disjoint: one pass, moving past each frame in the way.
                let mut s = i64::from(MIN_LEAD_US);
                for b in spans() {
                    if collides(b, s) {
                        s = b.1 + PRE;
                    }
                }
                if s > i64::from(MAX_ADVANCE_US) {
                    return Err(Error::TooEarly);
                }
                s
            }
        };
        let start = now.wrapping_add(start as u32);
        let i = self
            .entries
            .iter()
            .position(|e| offset(e.start, now) > offset(start, now))
            .unwrap_or(self.entries.len());
        let _ = self.entries.insert(i, Entry { start, airtime_us, item });
        Ok(start)
    }

    /// Start time of the first frame.
    pub fn next_start(&self) -> Option<u32> {
        self.entries.first().map(|e| e.start)
    }

    /// Removes the first frame if it starts within `HANDOVER_US` of `now` (or is overdue).
    /// Its airtime stays blocked until `tx_done`.
    pub fn pop_due(&mut self, now: u32) -> Option<(u32, T)> {
        let e = self.entries.first()?;
        if offset(e.start, now) > HANDOVER_US as i32 {
            return None;
        }
        let e = self.entries.remove(0);
        self.on_air = Some(Entry { start: e.start, airtime_us: e.airtime_us, item: () });
        Some((e.start, e.item))
    }

    /// The frame from `pop_due` has been sent, or dropped.
    pub fn tx_done(&mut self) {
        self.on_air = None;
    }
}

/// LoRa time on air in µs (Semtech AN1200.13), explicit header; `cr` 1–4 for 4/5–4/8.
/// `None` outside SF7–12 and 125/250/500 kHz, the parameters EU868 uses, and for a
/// preamble above 65535 symbols.
pub fn lora_airtime_us(
    bandwidth: u32,
    sf: u32,
    cr: u32,
    preamble: u32,
    crc: bool,
    len: usize,
) -> Option<u32> {
    // The SX1302 takes a 16-bit preamble length; anything longer would also overflow
    // the arithmetic below and undercount the airtime.
    if !matches!(bandwidth, 125_000 | 250_000 | 500_000)
        || !(7..=12).contains(&sf)
        || !(1..=4).contains(&cr)
        || preamble > u32::from(u16::MAX)
    {
        return None;
    }
    // Low data rate optimisation for symbols of 16 ms and more.
    let ldro = (1u64 << sf) * 1000 / u64::from(bandwidth / 1000) >= 16_000;
    let bits = 8 * len as i64 - 4 * i64::from(sf) + 28 + if crc { 16 } else { 0 };
    let per_block = 4 * (i64::from(sf) - if ldro { 2 } else { 0 });
    let blocks = (bits.max(0) + per_block - 1) / per_block;
    let payload_symbols = 8 + blocks as u64 * (u64::from(cr) + 4);
    // Preamble: n + 4.25 symbols, in quarter symbols.
    let quarter_symbols = 4 * u64::from(preamble) + 17 + 4 * payload_symbols;
    let us = quarter_symbols * (1u64 << sf) * 1_000_000 / 4 / u64::from(bandwidth);
    u32::try_from(us).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIR: u32 = 50_000;

    #[test]
    fn airtime() {
        // Semtech LoRa calculator values.
        assert_eq!(lora_airtime_us(125_000, 7, 1, 8, true, 13), Some(46_336));
        assert_eq!(lora_airtime_us(125_000, 12, 1, 8, true, 13), Some(1_155_072));
        assert_eq!(lora_airtime_us(125_000, 9, 1, 8, false, 12), Some(144_384));
        assert_eq!(lora_airtime_us(250_000, 7, 1, 8, true, 51), Some(51_328));
        assert_eq!(lora_airtime_us(125_000, 6, 1, 8, true, 13), None);
        assert_eq!(lora_airtime_us(203_000, 7, 1, 8, true, 13), None);
        // One SF7/125 kHz symbol is 1024 µs.
        assert_eq!(lora_airtime_us(125_000, 7, 1, 65_535, true, 13), Some(46_336 + (65_535 - 8) * 1024));
        assert_eq!(lora_airtime_us(125_000, 7, 1, 65_536, true, 13), None);
        assert_eq!(lora_airtime_us(125_000, 12, 1, u32::MAX, true, 13), None);
    }

    #[test]
    fn too_late_and_too_early() {
        let mut q: Queue<u8, 4> = Queue::new();
        let now = 1_000_000;
        assert_eq!(q.enqueue(now, At::Count(now + MIN_LEAD_US - 1), AIR, 0), Err(Error::TooLate));
        assert_eq!(q.enqueue(now, At::Count(now - 5), AIR, 0), Err(Error::TooLate));
        assert_eq!(q.enqueue(now, At::Count(now + MAX_ADVANCE_US + 1), AIR, 0), Err(Error::TooEarly));
        assert_eq!(q.enqueue(now, At::Count(now + MIN_LEAD_US), AIR, 0), Ok(now + MIN_LEAD_US));
        assert_eq!(q.enqueue(now, At::Count(now + MAX_ADVANCE_US), AIR, 1), Ok(now + MAX_ADVANCE_US));
    }

    #[test]
    fn collision() {
        let mut q: Queue<u8, 4> = Queue::new();
        let now = 0;
        let t = 1_000_000;
        q.enqueue(now, At::Count(t), AIR, 0).unwrap();
        // Needs AIR + margins after t, and the start delay + margins before it.
        let after = t + AIR + TX_MARGIN_US + TX_START_DELAY_US + TX_MARGIN_US;
        assert_eq!(q.enqueue(now, At::Count(after - 1), AIR, 1), Err(Error::Collision));
        assert_eq!(q.enqueue(now, At::Count(after), AIR, 1), Ok(after));
        let before = t - AIR - (TX_MARGIN_US + TX_START_DELAY_US + TX_MARGIN_US);
        assert_eq!(q.enqueue(now, At::Count(before + 1), AIR, 2), Err(Error::Collision));
        assert_eq!(q.enqueue(now, At::Count(before), AIR, 2), Ok(before));
        assert_eq!(q.enqueue(now, At::Count(5_000_000), AIR, 3), Ok(5_000_000));
        assert_eq!(q.enqueue(now, At::Count(9_000_000), AIR, 4), Err(Error::Full));
    }

    #[test]
    fn asap_finds_a_gap() {
        let mut q: Queue<u8, 4> = Queue::new();
        let now = 0;
        assert_eq!(q.enqueue(now, At::Asap, AIR, 0), Ok(MIN_LEAD_US));
        let gap = AIR + TX_MARGIN_US + TX_START_DELAY_US + TX_MARGIN_US;
        // Too small a gap behind the first frame, so the next one goes behind the second.
        let second = MIN_LEAD_US + 2 * gap - 1;
        q.enqueue(now, At::Count(second), AIR, 1).unwrap();
        assert_eq!(q.enqueue(now, At::Asap, AIR, 2), Ok(second + gap));
        // A short frame fits into the gap.
        assert_eq!(q.enqueue(now, At::Asap, 1_000, 3), Ok(MIN_LEAD_US + gap));
    }

    #[test]
    fn order_and_handover() {
        let mut q: Queue<u8, 4> = Queue::new();
        let now = 0;
        q.enqueue(now, At::Count(3_000_000), AIR, 3).unwrap();
        q.enqueue(now, At::Count(1_000_000), AIR, 1).unwrap();
        q.enqueue(now, At::Count(2_000_000), AIR, 2).unwrap();
        assert_eq!(q.next_start(), Some(1_000_000));
        assert_eq!(q.pop_due(1_000_000 - HANDOVER_US - 1), None);
        assert_eq!(q.pop_due(1_000_000 - HANDOVER_US), Some((1_000_000, 1)));
        // Overdue frames still come out; the caller decides what to do with them.
        assert_eq!(q.pop_due(2_500_000), Some((2_000_000, 2)));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn on_air_blocks_until_done() {
        let mut q: Queue<u8, 4> = Queue::new();
        let now = 0;
        q.enqueue(now, At::Asap, AIR, 0).unwrap();
        let now = MIN_LEAD_US - HANDOVER_US;
        assert_eq!(q.pop_due(now), Some((MIN_LEAD_US, 0)));
        // The popped frame still occupies its airtime.
        let after = MIN_LEAD_US + AIR + TX_MARGIN_US + TX_START_DELAY_US + TX_MARGIN_US;
        assert_eq!(q.enqueue(now, At::Count(after - 1), AIR, 1), Err(Error::Collision));
        assert_eq!(q.enqueue(now, At::Asap, AIR, 2), Ok(after));
        // Done: a short frame fits in front of the queued one again.
        q.tx_done();
        assert_eq!(q.enqueue(now, At::Asap, 1_000, 3), Ok(now + MIN_LEAD_US));
    }

    #[test]
    fn counter_wrap() {
        let mut q: Queue<u8, 4> = Queue::new();
        let now = u32::MAX - 100_000;
        let t = now.wrapping_add(1_000_000);
        assert_eq!(q.enqueue(now, At::Count(t), AIR, 0), Ok(t));
        // Behind the wrap, but earlier in time.
        assert_eq!(q.enqueue(now, At::Count(u32::MAX - 20_000), AIR, 1), Ok(u32::MAX - 20_000));
        assert_eq!(q.next_start(), Some(u32::MAX - 20_000));
        assert_eq!(q.enqueue(now, At::Count(t + AIR), AIR, 2), Err(Error::Collision));
        assert_eq!(q.pop_due(t - HANDOVER_US), Some((u32::MAX - 20_000, 1)));
        assert_eq!(q.pop_due(t - HANDOVER_US), Some((t, 0)));
    }
}
