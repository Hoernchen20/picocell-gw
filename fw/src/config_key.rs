//! Source of the config key (sketch.md section 8.1, milestone 8d).
//!
//! Dev builds derive the key from the chip ID, as before. Production builds derive it
//! from 16 random bytes in OTP page `PAGE`, written once by `otp-key init` on the
//! console: the key in eight ECC rows, then a 32-bit check (the start of its SHA-256),
//! written last, so an interrupted write reads as damaged instead of as a weaker key.
//! Without a valid key the gateway stays in provisioning mode, and nothing can be
//! stored.
//!
//! Both builds read the page once at boot and then soft-lock it until the next reset
//! (`SW_LOCK`), so nothing running later, the console included, can read it again; the
//! dev boards test that path without writing OTP. Only a production build with an
//! empty page leaves it readable to secure code, for `otp-key init`, which locks it
//! after writing. `otp-key init` also programs the page's permanent locks: secure
//! read-only, no non-secure access, and no bootloader access (so `picotool otp` in
//! BOOTSEL cannot read it). The key itself never leaves the device.

use core::cell::Cell;
use core::fmt;

use critical_section::Mutex;
use embassy_crypto::{HmacSha256, Sha256};
use embassy_rp::otp;
use embassy_rp::pac;
use embassy_rp::pac::otp::vals::{SwLockNsec, SwLockSec};

/// OTP page of the key (rows 0x500 to 0x53f), in the user area (pages 3 to 60) and
/// away from page 30, picotool's default for encrypted boot.
pub const PAGE: usize = 20;
const FIRST_ROW: usize = PAGE * otp::NUM_ROWS_PER_PAGE;
const KEY: usize = 16;
/// ECC rows hold 16 bits each.
const KEY_ROWS: usize = KEY / 2;
const CHECK_ROWS: usize = 2;
/// Permanent locks of page n: LOCK0 in row 0xf80 + 2n, LOCK1 in the row after it.
#[cfg(feature = "production")]
const LOCK1_ROW: usize = 0xf80 + 2 * PAGE + 1;

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    /// Not read yet.
    Unknown,
    /// All rows zero: no key written.
    Empty,
    Key,
    /// Written, but the check does not match.
    Damaged,
    /// The read was refused.
    Unreadable,
}

static PAGE_STATE: Mutex<Cell<Page>> = Mutex::new(Cell::new(Page::Unknown));
/// A config key is in use.
static LOADED: Mutex<Cell<bool>> = Mutex::new(Cell::new(false));

fn set_state(p: Page) {
    critical_section::with(|cs| PAGE_STATE.borrow(cs).set(p));
}

/// The key page as read at boot (or written by `init_otp`).
pub fn page() -> Page {
    critical_section::with(|cs| PAGE_STATE.borrow(cs).get())
}

fn check(key: &[u8; KEY]) -> [u16; CHECK_ROWS] {
    let d = Sha256::digest(key);
    [u16::from_le_bytes([d[0], d[1]]), u16::from_le_bytes([d[2], d[3]])]
}

fn read_page() -> (Page, Option<[u8; KEY]>) {
    let mut empty = true;
    for row in FIRST_ROW..FIRST_ROW + otp::NUM_ROWS_PER_PAGE {
        match otp::read_raw_word(row) {
            Ok(w) => empty &= w == 0,
            Err(_) => return (Page::Unreadable, None),
        }
    }
    if empty {
        return (Page::Empty, None);
    }
    let mut rows = [0u16; KEY_ROWS + CHECK_ROWS];
    for (i, r) in rows.iter_mut().enumerate() {
        match otp::read_ecc_word(FIRST_ROW + i) {
            Ok(w) => *r = w,
            Err(_) => return (Page::Unreadable, None),
        }
    }
    let mut key = [0u8; KEY];
    for (i, r) in rows[..KEY_ROWS].iter().enumerate() {
        key[2 * i..2 * i + 2].copy_from_slice(&r.to_le_bytes());
    }
    if rows[KEY_ROWS..] == check(&key) { (Page::Key, Some(key)) } else { (Page::Damaged, None) }
}

/// Until the next reset; writes are OR'd into the lock state, so this only tightens.
fn soft_lock(secure: bool) {
    pac::OTP.sw_lock(PAGE).write(|w| {
        w.set_nsec(SwLockNsec::INACCESSIBLE);
        if secure {
            w.set_sec(SwLockSec::INACCESSIBLE);
        }
    });
}

/// Whether a read of the page is refused right now.
pub fn soft_locked() -> bool {
    otp::read_raw_word(FIRST_ROW).is_err()
}

/// Reads and soft-locks the key page; returns the config key, or `None` (provisioning
/// mode). Called once at boot.
pub fn load() -> Option<[u8; KEY]> {
    let (page, otp_key) = read_page();
    set_state(page);
    let production = cfg!(feature = "production");
    soft_lock(!(production && page == Page::Empty));
    let key = if production {
        // Its own derivation, so other keys can come from the same OTP bytes later.
        otp_key.map(|k| HmacSha256::mac(&k, b"picocell-gw config key")[..KEY].try_into().unwrap())
    } else {
        let chip_id = otp::get_chipid().unwrap_or(0);
        Some(HmacSha256::mac(b"picocell-gw dev config key", &chip_id.to_be_bytes())[..KEY].try_into().unwrap())
    };
    critical_section::with(|cs| LOADED.borrow(cs).set(key.is_some()));
    key
}

/// Where the config key comes from: "dev", "otp" or "none".
pub fn source() -> &'static str {
    match critical_section::with(|cs| LOADED.borrow(cs).get()) {
        false => "none",
        true if cfg!(feature = "production") => "otp",
        true => "dev",
    }
}

/// For `status`: source, page state and soft lock.
pub struct Status;

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match source() {
            "dev" => "chip ID (dev build)",
            "otp" => "OTP",
            _ => "none, provisioning mode",
        };
        let page = match page() {
            Page::Unknown => "not read",
            Page::Empty => "empty",
            Page::Key => "key",
            Page::Damaged => "damaged",
            Page::Unreadable => "unreadable",
        };
        let lock = if soft_locked() { "soft-locked" } else { "readable" };
        write!(f, "{source}; OTP page {PAGE} {page}, {lock}")
    }
}

/// Why `otp-key init` cannot run, if it cannot.
pub fn can_init() -> Result<(), &'static str> {
    if !cfg!(feature = "production") {
        return Err("dev build: OTP writes are compiled out");
    }
    if !crate::boot::security().secure_boot {
        return Err("secure boot is off, so an unsigned image could read the key");
    }
    match page() {
        Page::Empty => Ok(()),
        Page::Key => Err("the key is already written"),
        _ => Err("OTP key page not empty"),
    }
}

#[cfg(not(feature = "production"))]
pub fn init_otp() -> Result<(), &'static str> {
    can_init()
}

/// Writes a new random key into the empty page, reads it back, programs the permanent
/// page locks and soft-locks the page. Takes effect after a restart. Cannot be undone.
#[cfg(feature = "production")]
pub fn init_otp() -> Result<(), &'static str> {
    use embassy_rp::pac::otp_data_raw::vals::PageLock;

    can_init()?;
    if otp::read_raw_word(LOCK1_ROW).ok() != Some(0) {
        return Err("OTP key page already has permanent locks");
    }
    let mut key = [0u8; KEY];
    crate::tls::random_bytes(&mut key);
    if key == [0; KEY] {
        return Err("TRNG returned zeros");
    }
    let words = key.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    for (i, w) in words.chain(check(&key)).enumerate() {
        otp::write_ecc_word(FIRST_ROW + i, w).map_err(|_| "OTP write failed, page damaged")?;
    }
    let (page, back) = read_page();
    set_state(page);
    if back != Some(key) {
        return Err("read-back differs, page damaged");
    }
    // Secure read-only, no non-secure access, no bootloader access; three copies of the
    // byte, majority-voted.
    let lock = PageLock::READ_ONLY.to_bits() | PageLock::INACCESSIBLE.to_bits() << 2 | PageLock::INACCESSIBLE.to_bits() << 4;
    let lock = u32::from(lock) * 0x01_0101;
    let locked = otp::write_raw_word(LOCK1_ROW, lock).is_ok() && otp::read_raw_word(LOCK1_ROW).ok() == Some(lock);
    soft_lock(true);
    // Not kept in RAM; the next boot reads it from OTP.
    for b in key.iter_mut() {
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    if !locked {
        return Err("key written, but the permanent page locks failed");
    }
    Ok(())
}
