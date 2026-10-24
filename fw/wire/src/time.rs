//! Unix time formatting for logs.

/// Formats Unix seconds as `YYYY-MM-DD hh:mm:ss UTC`.
pub struct Utc(pub u64);

impl core::fmt::Display for Utc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (days, secs) = (self.0 / 86_400, self.0 % 86_400);
        let (y, m, d) = civil_from_days(days);
        write!(
            f,
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
    }
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian calendar
/// (H. Hinnant, "chrono-Compatible Low-Level Date Algorithms").
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    #[test]
    fn format() {
        assert_eq!(Utc(0).to_string(), "1970-01-01 00:00:00 UTC");
        assert_eq!(Utc(951_782_400).to_string(), "2000-02-29 00:00:00 UTC");
        assert_eq!(Utc(1_792_849_845).to_string(), "2026-10-24 13:50:45 UTC");
        assert_eq!(Utc(4_107_542_399).to_string(), "2100-02-28 23:59:59 UTC");
    }
}
