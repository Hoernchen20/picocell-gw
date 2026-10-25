//! Boot ROM side of the image (sketch.md section 8.1, milestones 8a and 8b): own
//! IMAGE_DEF, what the ROM reports about this boot, trial boots and the watchdog.
//!
//! The IMAGE_DEF marks the image as a secure ARM executable with the TBYB ("try before
//! you buy") flag. After a flash update (UF2 copy or OTA reboot) the ROM boots such an
//! image as a trial with a 16.7 s watchdog. `explicit_buy` clears the flag and erases
//! the first sector of the other slot, so the image is the only one left to boot;
//! without it any reboot boots the other slot again, since a normal boot always
//! prefers an image without the flag. The version and hash items are added when the
//! image is sealed (`picotool seal`), not here.
//!
//! An OTA update (see `ota`) is bought after the first broker connection; missing that
//! for `BUY_DEADLINE_S`, the gateway reboots into the old image. A UF2 copy needs
//! someone at the USB port, the recovery path anyway, so that trial is bought at boot
//! (the ROM reports both as a flash update boot; a mark in a watchdog scratch register
//! tells them apart).

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_rp::block::{Architecture, IMAGE_TYPE_TBYB, ImageDef, Security, item_image_type_exe};
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{pac, rom_data};
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};

use crate::flash;

// embassy-rp's own IMAGE_DEF is off (`imagedef-none`).
#[unsafe(link_section = ".start_block")]
#[used]
static IMAGE_DEF: ImageDef =
    ImageDef::new([item_image_type_exe(Security::Secure, Architecture::Arm) | ((IMAGE_TYPE_TBYB as u32) << 16)]);

// `get_sys_info` and `reboot` (pico-sdk boot/bootrom_constants.h, picoboot_constants.h).
const SYS_INFO_BOOT_INFO: u32 = 0x0040;
const BOOT_TYPE_CHAINED: u8 = 0x80;
const BUY_PENDING: u8 = 0x01;
const REBOOT_TYPE_FLASH_UPDATE: u32 = 0x4;
const REBOOT_NO_RETURN_ON_SUCCESS: u32 = 0x100;

/// In watchdog SCRATCH0 across an OTA reboot ("OTA1"). SCRATCH0 and 1 are free for the
/// application; the ROM uses 2 to 7.
const OTA_MARK: u32 = 0x4f54_4131;
/// An OTA trial that has not reached the broker by then reboots into the old image.
pub const BUY_DEADLINE_S: u64 = 300;
/// Fed every second by `watchdog_task`; the hardware limit is 16.7 s.
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(8);

/// An OTA trial waiting for the broker connection.
static TRIAL: AtomicBool = AtomicBool::new(false);

/// The ROM's `boot_info_t`.
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// Partition index in the table, or negative (`BOOT_PARTITION_*`).
    pub partition: i8,
    pub boot_type: u8,
    pub tbyb_and_update: u8,
    pub diagnostic: u32,
}

pub fn info() -> Option<BootInfo> {
    let mut w = [0u32; 5];
    let n = unsafe { rom_data::get_sys_info(w.as_mut_ptr(), w.len(), SYS_INFO_BOOT_INFO) };
    if n != w.len() as i32 || w[0] != SYS_INFO_BOOT_INFO {
        return None;
    }
    let b = w[1].to_le_bytes();
    Some(BootInfo { partition: b[2] as i8, boot_type: b[1], tbyb_and_update: b[3], diagnostic: w[2] })
}

impl fmt::Display for BootInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // BOOT_PARTITION_* for boots without a partition table.
        match self.partition {
            0 => f.write_str("slot A")?,
            1 => f.write_str("slot B")?,
            -1 => f.write_str("no partition")?,
            -2 => f.write_str("flash start, no partition table")?,
            -3 => f.write_str("flash slot 1, no partition table")?,
            -4 => f.write_str("flash window")?,
            p => write!(f, "partition {p}")?,
        }
        let kind = match self.boot_type & !BOOT_TYPE_CHAINED {
            0 => "normal",
            2 => "bootsel",
            3 => "ram image",
            4 => "flash update",
            0xd => "pc/sp",
            _ => "unknown",
        };
        write!(
            f,
            ", {kind} boot (type {:#04x}), tbyb/update {:#04x}, diagnostic {:#010x}",
            self.boot_type, self.tbyb_and_update, self.diagnostic
        )
    }
}

/// Logs how the ROM booted the image; buys a trial boot unless it came from an OTA
/// update, which `confirm()` buys later.
pub fn start() {
    let ota = pac::WATCHDOG.scratch0().read() == OTA_MARK;
    pac::WATCHDOG.scratch0().write_value(0);
    let Some(i) = info() else {
        warn!("boot: no boot info from the ROM");
        return;
    };
    info!("boot: {i}");
    if i.tbyb_and_update & BUY_PENDING == 0 {
        return;
    }
    if ota {
        TRIAL.store(true, Ordering::Relaxed);
        info!("boot: OTA trial, bought once the broker connects, else the old image boots in {BUY_DEADLINE_S} s");
    } else {
        buy();
    }
}

/// Whether this is an OTA trial not bought yet.
pub fn trial() -> bool {
    TRIAL.load(Ordering::Relaxed)
}

/// Buys an OTA trial: the gateway reached the broker. Failed, the trial stays and the
/// deadline reboots into the old image.
pub fn confirm() {
    if trial() && buy() {
        TRIAL.store(false, Ordering::Relaxed);
    }
}

fn buy() -> bool {
    // Work area for rewriting the image's first sector without the TBYB flag.
    let mut work = [0u8; flash::SECTOR as usize];
    match flash::with_xip_idle(|| unsafe { rom_data::explicit_buy(work.as_mut_ptr(), work.len() as u32) }) {
        0 => {
            info!("explicit_buy: ok");
            true
        }
        e => {
            warn!("explicit_buy: error {e}; a reboot boots the other slot");
            false
        }
    }
}

/// Reboots into a new image in the slot at physical offset `slot_start` as a trial
/// (FLASH_UPDATE boot, so the ROM prefers that slot regardless of version).
pub fn reboot_into(slot_start: u32) -> ! {
    pac::WATCHDOG.scratch0().write_value(OTA_MARK);
    let flags = REBOOT_TYPE_FLASH_UPDATE | REBOOT_NO_RETURN_ON_SUCCESS;
    // The ROM takes the XIP address; a delay of 0 hits a ROM bug (pico-sdk).
    let e = rom_data::reboot(flags, 10, flash::XIP_BASE + slot_start, 0);
    warn!("reboot: error {e}");
    pac::WATCHDOG.scratch0().write_value(0);
    cortex_m::peripheral::SCB::sys_reset()
}

/// Feeds the watchdog while the executor runs; this also replaces the ROM's 16.7 s
/// trial watchdog. Reboots an OTA trial still not bought after `BUY_DEADLINE_S`.
#[embassy_executor::task]
pub async fn watchdog_task(mut wd: Watchdog) -> ! {
    wd.pause_on_debug(true);
    wd.start(WATCHDOG_TIMEOUT);
    loop {
        Timer::after_secs(1).await;
        if trial() && Instant::now().as_secs() >= BUY_DEADLINE_S {
            warn!("boot: OTA trial not bought within {BUY_DEADLINE_S} s, rebooting into the old image");
            Timer::after_millis(200).await;
            cortex_m::peripheral::SCB::sys_reset();
        }
        wd.feed(WATCHDOG_TIMEOUT);
    }
}
