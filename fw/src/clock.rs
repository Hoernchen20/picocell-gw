//! Wall clock: Unix time = base + `Instant::now()`, with the base set by SNTP.

use core::fmt;
use core::sync::atomic::Ordering;

use embassy_time::Instant;
use portable_atomic::AtomicI64;
use wire::time::Utc;

/// Unix µs at `Instant` 0; 0 = unknown (sketch.md section 6: fail closed).
static BASE_US: AtomicI64 = AtomicI64::new(0);

pub fn set(base_us: i64) {
    BASE_US.store(base_us, Ordering::Relaxed);
}

pub fn base_us() -> Option<i64> {
    match BASE_US.load(Ordering::Relaxed) {
        0 => None,
        b => Some(b),
    }
}

pub fn is_set() -> bool {
    base_us().is_some()
}

pub fn now_unix_s() -> Option<u64> {
    let us = base_us()? + Instant::now().as_micros() as i64;
    u64::try_from(us / 1_000_000).ok()
}

/// The current time for logs, or "time unknown".
pub struct Now;

impl fmt::Display for Now {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match now_unix_s() {
            Some(t) => Utc(t).fmt(f),
            None => f.write_str("time unknown"),
        }
    }
}
