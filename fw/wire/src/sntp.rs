//! SNTP v4 client messages (RFC 4330) and clock offset calculation.
//!
//! Times on the local side are microseconds of a monotonic counter; the result is the
//! Unix time in microseconds at counter value 0 ("base"), so Unix time = base + counter.

pub const PORT: u16 = 123;
pub const LEN: usize = 48;

/// Seconds from 1900-01-01 (NTP era 0) to 1970-01-01.
const NTP_UNIX_OFFSET: i64 = 2_208_988_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Shorter than an NTP header or not a server reply.
    Malformed,
    /// Origin timestamp is not our cookie: a stale or forged reply; ignore it.
    Mismatch,
    /// The server is not synchronised (leap indicator 3, stratum 0 / kiss-o'-death or
    /// stratum > 15, zero transmit time).
    Unsynchronized,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Malformed => "malformed reply",
            Error::Mismatch => "reply does not match the request",
            Error::Unsynchronized => "server not synchronised",
        })
    }
}

/// A client request. The transmit timestamp carries a random `cookie` instead of the
/// (unknown) local time; the server echoes it as origin timestamp.
pub fn request(cookie: [u8; 8]) -> [u8; LEN] {
    let mut m = [0u8; LEN];
    m[0] = 4 << 3 | 3; // LI 0, version 4, mode 3 (client)
    m[40..48].copy_from_slice(&cookie);
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub stratum: u8,
    /// Server time when the request arrived (T2), Unix µs.
    pub receive_us: i64,
    /// Server time when the reply left (T3), Unix µs.
    pub transmit_us: i64,
}

pub fn parse_reply(cookie: [u8; 8], msg: &[u8]) -> Result<Reply, Error> {
    let msg: &[u8; LEN] = msg.get(..LEN).ok_or(Error::Malformed)?.try_into().unwrap();
    let leap = msg[0] >> 6;
    let version = (msg[0] >> 3) & 7;
    let mode = msg[0] & 7;
    let stratum = msg[1];
    if mode != 4 || !(3..=4).contains(&version) {
        return Err(Error::Malformed);
    }
    if msg[24..32] != cookie {
        return Err(Error::Mismatch);
    }
    if leap == 3 || stratum == 0 || stratum > 15 || msg[40..48] == [0; 8] {
        return Err(Error::Unsynchronized);
    }
    Ok(Reply {
        stratum,
        receive_us: timestamp_us(&msg[32..40]),
        transmit_us: timestamp_us(&msg[40..48]),
    })
}

/// NTP timestamp → Unix µs. Seconds with the top bit clear are taken as era 1
/// (from 2036-02-07), which is valid until 2104.
fn timestamp_us(ts: &[u8]) -> i64 {
    let secs = u32::from_be_bytes(ts[..4].try_into().unwrap());
    let frac = u32::from_be_bytes(ts[4..].try_into().unwrap());
    let mut secs = i64::from(secs);
    if secs & 0x8000_0000 == 0 {
        secs += 1 << 32;
    }
    let us = (u64::from(frac) * 1_000_000) >> 32;
    (secs - NTP_UNIX_OFFSET) * 1_000_000 + us as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Unix µs at local counter 0.
    pub base_us: i64,
    /// Round-trip delay without the server's processing time, µs.
    pub delay_us: i64,
}

/// Offset from one exchange: request sent at local `t1`, reply received at local `t4`.
pub fn sample(t1_us: u64, t4_us: u64, r: &Reply) -> Sample {
    let (t1, t4) = (t1_us as i64, t4_us as i64);
    Sample {
        base_us: ((r.receive_us - t1) + (r.transmit_us - t4)) / 2,
        delay_us: (t4 - t1) - (r.transmit_us - r.receive_us),
    }
}

/// Median of the samples, so one bad server out of three is outvoted. With an even
/// count, the one of the two middle samples with the lower delay.
pub fn combine(samples: &mut [Sample]) -> Option<Sample> {
    samples.sort_unstable_by_key(|s| s.base_us);
    let n = samples.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(samples[n / 2]),
        _ => Some(*samples[n / 2 - 1..=n / 2].iter().min_by_key(|s| s.delay_us).unwrap()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    /// 2026-10-24 00:00:00 UTC.
    const UNIX: i64 = 1_792_800_000;

    fn ntp(unix_us: i64) -> [u8; 8] {
        let secs = (unix_us.div_euclid(1_000_000) + NTP_UNIX_OFFSET) as u64 as u32;
        let frac = ((unix_us.rem_euclid(1_000_000) as u64) << 32) / 1_000_000;
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&secs.to_be_bytes());
        b[4..].copy_from_slice(&(frac as u32).to_be_bytes());
        b
    }

    fn reply(t2: i64, t3: i64) -> [u8; LEN] {
        let mut m = [0u8; LEN];
        m[0] = 4 << 3 | 4;
        m[1] = 2;
        m[24..32].copy_from_slice(&COOKIE);
        m[32..40].copy_from_slice(&ntp(t2));
        m[40..48].copy_from_slice(&ntp(t3));
        m
    }

    #[test]
    fn request_layout() {
        let m = request(COOKIE);
        assert_eq!(m[0], 0x23);
        assert_eq!(m[40..48], COOKIE);
        assert!(m[1..40].iter().all(|b| *b == 0));
    }

    #[test]
    fn reply_and_offset() {
        let t2 = UNIX * 1_000_000 + 250_000;
        let t3 = t2 + 1_000;
        let r = parse_reply(COOKIE, &reply(t2, t3)).unwrap();
        assert_eq!(r.stratum, 2);
        // Fraction conversion loses at most 1 µs.
        assert!((r.receive_us - t2).abs() <= 1);
        assert!((r.transmit_us - t3).abs() <= 1);

        // 10 ms each way, server took 1 ms; the request arrived at local counter 5 s.
        let r = Reply { stratum: 2, receive_us: t2, transmit_us: t3 };
        let s = sample(5_000_000 - 10_000, 5_001_000 + 10_000, &r);
        assert_eq!(s.delay_us, 20_000);
        assert_eq!(s.base_us, t2 - 5_000_000);
    }

    #[test]
    fn era_1() {
        // 2040-01-01: NTP seconds have wrapped past 2^32.
        let unix = 2_208_988_800i64;
        let r = parse_reply(COOKIE, &reply(unix * 1_000_000, unix * 1_000_000)).unwrap();
        assert_eq!(r.transmit_us, unix * 1_000_000);
    }

    #[test]
    fn rejects() {
        let good = reply(UNIX * 1_000_000, UNIX * 1_000_000);
        assert_eq!(parse_reply([0; 8], &good), Err(Error::Mismatch));
        assert_eq!(parse_reply(COOKIE, &good[..47]), Err(Error::Malformed));
        let mut m = good;
        m[0] = 4 << 3 | 3; // client mode, e.g. our own request reflected
        assert_eq!(parse_reply(COOKIE, &m), Err(Error::Malformed));
        let mut m = good;
        m[0] |= 3 << 6;
        assert_eq!(parse_reply(COOKIE, &m), Err(Error::Unsynchronized));
        let mut m = good;
        m[1] = 0;
        assert_eq!(parse_reply(COOKIE, &m), Err(Error::Unsynchronized));
        let mut m = good;
        m[1] = 16;
        assert_eq!(parse_reply(COOKIE, &m), Err(Error::Unsynchronized));
        let mut m = good;
        m[40..48].fill(0);
        assert_eq!(parse_reply(COOKIE, &m), Err(Error::Unsynchronized));
    }

    #[test]
    fn median() {
        let s = |base_us, delay_us| Sample { base_us, delay_us };
        assert_eq!(combine(&mut []), None);
        assert_eq!(combine(&mut [s(5, 1)]), Some(s(5, 1)));
        // An outlier (one hour off) is outvoted.
        assert_eq!(combine(&mut [s(3_600_000_000, 1), s(10, 5), s(12, 9)]), Some(s(12, 9)));
        assert_eq!(combine(&mut [s(12, 9), s(10, 5)]), Some(s(10, 5)));
    }
}
