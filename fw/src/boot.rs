//! Boot ROM side of the image (sketch.md section 8.1, milestone 8a): own IMAGE_DEF and
//! what the ROM reports about this boot.
//!
//! The IMAGE_DEF marks the image as a secure ARM executable with the TBYB ("try before
//! you buy") flag. After a flash update (UF2 copy or OTA reboot) the ROM boots such an
//! image as a trial with a 16.7 s watchdog and falls back to the other slot unless the
//! image calls `explicit_buy`. Until OTA (8b) moves the buy behind the first broker
//! connection, `buy()` runs right at boot. The version and hash items are added when
//! the image is sealed (`picotool seal`), not here.

use core::fmt;

use embassy_rp::block::{Architecture, IMAGE_TYPE_TBYB, ImageDef, Security, item_image_type_exe};
use embassy_rp::rom_data;
use log::{info, warn};

use crate::flash;

// embassy-rp's own IMAGE_DEF is off (`imagedef-none`).
#[unsafe(link_section = ".start_block")]
#[used]
static IMAGE_DEF: ImageDef =
    ImageDef::new([item_image_type_exe(Security::Secure, Architecture::Arm) | ((IMAGE_TYPE_TBYB as u32) << 16)]);

// `get_sys_info` (pico-sdk boot/bootrom_constants.h).
const SYS_INFO_BOOT_INFO: u32 = 0x0040;
const BOOT_TYPE_CHAINED: u8 = 0x80;
const BUY_PENDING: u8 = 0x01;

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

/// Logs how the ROM booted the image and buys a trial boot.
pub fn buy() {
    let Some(i) = info() else {
        warn!("boot: no boot info from the ROM");
        return;
    };
    info!("boot: {i}");
    if i.tbyb_and_update & BUY_PENDING == 0 {
        return;
    }
    // Work area for rewriting the image's first sector without the TBYB flag.
    let mut work = [0u8; flash::SECTOR as usize];
    match flash::with_xip_idle(|| unsafe { rom_data::explicit_buy(work.as_mut_ptr(), work.len() as u32) }) {
        0 => info!("explicit_buy: ok"),
        e => warn!("explicit_buy: error {e}; the ROM falls back to the other slot after 16.7 s"),
    }
}
