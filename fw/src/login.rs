//! Console login (sketch.md section 5.5): with `console.password` in the stored config,
//! the console only offers `help`, `status`, `log`, `login`, `logout` and `erase` until
//! the password is typed. The password is kept as PBKDF2-HMAC-SHA256 with a random
//! salt (`settings::PasswordHash`), so a flash dump with the config key does not reveal
//! it either. Each wrong password doubles the wait before the next try (3 s up to
//! 60 s); the count only resets on a correct password or a restart.
//!
//! A forgotten password: `erase` (no login needed) deletes the whole config, the MQTT
//! password included, so nothing is given away; then provision the board again.

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_crypto::HmacSha256;
use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use settings::PasswordHash;

use crate::{config, tls};

/// For new hashes; 0.5 s on the RP2350 at 150 MHz. A stored hash brings its own
/// count (up to `PasswordHash::MAX_ITERATIONS`).
pub const ITERATIONS: u32 = 10_000;

/// Wrong passwords since the last correct one or the restart.
static FAILURES: AtomicU8 = AtomicU8::new(0);

/// A login is required: the stored config has a console password.
pub fn required() -> bool {
    config::get().console_password().is_some()
}

/// PBKDF2-HMAC-SHA256, one 32-byte block; yields now and then so the other tasks keep
/// running.
async fn pbkdf2(password: &[u8], salt: &[u8; 16], iterations: u32) -> [u8; 32] {
    let keyed = HmacSha256::new(password);
    let mut m = keyed.clone();
    m.update(salt);
    m.update(&1u32.to_be_bytes());
    let mut u = m.finalize();
    let mut t = u;
    for i in 1..iterations {
        let mut m = keyed.clone();
        m.update(&u);
        u = m.finalize();
        for (t, u) in t.iter_mut().zip(u) {
            *t ^= u;
        }
        if i % 256 == 0 {
            yield_now().await;
        }
    }
    t
}

/// The stored form of a new password, with a fresh salt.
pub async fn hash(password: &str) -> PasswordHash {
    let mut salt = [0; 16];
    tls::random_bytes(&mut salt);
    let start = Instant::now();
    let hash = pbkdf2(password.as_bytes(), &salt, ITERATIONS).await;
    info!("console password hashed in {} ms", start.elapsed().as_millis());
    PasswordHash { iterations: ITERATIONS, salt, hash }
}

/// Checks `password` against the stored hash; after a wrong one waits before
/// answering.
pub async fn check(password: &str) -> Result<(), &'static str> {
    let stored = config::get().console_password().ok_or("no console password set")?;
    let got = pbkdf2(password.as_bytes(), &stored.salt, stored.iterations).await;
    // Constant time.
    if got.iter().zip(stored.hash).fold(0, |d, (a, b)| d | (a ^ b)) == 0 {
        FAILURES.store(0, Ordering::Relaxed);
        info!("console: logged in");
        return Ok(());
    }
    let n = FAILURES.load(Ordering::Relaxed).saturating_add(1);
    FAILURES.store(n, Ordering::Relaxed);
    warn!("console: wrong password ({n} in a row)");
    Timer::after(delay(n)).await;
    Err("wrong password")
}

/// Wait after the `n`th wrong password in a row.
fn delay(n: u8) -> Duration {
    Duration::from_secs((3u64 << (n - 1).min(5)).min(60))
}
