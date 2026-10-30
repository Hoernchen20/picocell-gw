//! Flash access through the boot ROM's `flash_op` (sketch.md section 3.1), as an
//! `embedded-storage` driver for one flash partition.
//!
//! With the A/B partition table the ROM maps the running slot to 0x10000000, so XIP
//! reads (and `embassy_rp::flash::Flash::read`) no longer reach the config partition
//! or the other slot. `flash_op` works on storage addresses (physical flash offsets in
//! the XIP window) and checks the partition permissions for the secure caller. It
//! turns XIP off while it runs, so every call runs in a critical section, after DMA
//! and XIP streaming reads from flash have finished, with core 1 (the radio) parked in
//! RAM (as in embassy-rp's flash driver).

use core::ops::Range;

use embassy_rp::{pac, rom_data};
use embedded_storage_async::nor_flash::{
    ErrorType, MultiwriteNorFlash, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

pub const XIP_BASE: u32 = 0x1000_0000;
const PAGE: usize = 256;
pub const SECTOR: u32 = 4096;

// `flash_op` flags (pico-sdk boot/bootrom_constants.h): operation (bits 16..18),
// security level (bits 8..9), address space (bit 0).
const OP_ERASE: u32 = 0 << 16;
const OP_PROGRAM: u32 = 1 << 16;
const OP_READ: u32 = 2 << 16;
const SECLEVEL_SECURE: u32 = 1 << 8;
const ASPACE_STORAGE: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    OutOfBounds,
    NotAligned,
    /// A negative `BOOTROM_ERROR_*` code, e.g. -4 (not permitted by the partition).
    Rom(i32),
}

impl NorFlashError for Error {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            Error::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            Error::NotAligned => NorFlashErrorKind::NotAligned,
            Error::Rom(_) => NorFlashErrorKind::Other,
        }
    }
}

/// Runs `f` with core 1 parked, interrupts off and no DMA or XIP stream reading flash,
/// for ROM calls that turn XIP off (`flash_op`, `explicit_buy`). Core 0 only, and not
/// from inside a critical section (core 1 may be waiting for it to take the pause).
pub fn with_xip_idle<R>(f: impl FnOnce() -> R) -> R {
    embassy_rp::multicore::pause_core1();
    let r = critical_section::with(|_| {
        for n in 0..16 {
            let ch = pac::DMA.ch(n);
            while ch.read_addr().read() < 0x2000_0000 && ch.ctrl_trig().read().busy() {}
        }
        while pac::XIP_CTRL.stream_ctr().read().0 > 0 {}
        f()
    });
    embassy_rp::multicore::resume_core1();
    r
}

fn flash_op(op: u32, addr: u32, buf: *mut u8, len: usize) -> Result<(), Error> {
    let flags = op | ASPACE_STORAGE | SECLEVEL_SECURE;
    match with_xip_idle(|| unsafe { rom_data::flash_op(flags, XIP_BASE + addr, len as u32, buf) }) {
        0 => Ok(()),
        e => {
            log::warn!("flash_op {:#x} at {addr:#x} ({len} bytes): error {e}", op >> 16);
            Err(Error::Rom(e))
        }
    }
}

/// A flash range by physical offset; the driver's offsets are relative to its start.
pub struct Partition {
    start: u32,
    size: u32,
}

impl Partition {
    pub const fn new(range: Range<u32>) -> Self {
        assert!(range.start.is_multiple_of(SECTOR) && range.end.is_multiple_of(SECTOR) && range.start < range.end);
        Self { start: range.start, size: range.end - range.start }
    }

    fn check(&self, offset: u32, len: usize) -> Result<u32, Error> {
        match offset.checked_add(len as u32) {
            Some(end) if len <= self.size as usize && end <= self.size => Ok(self.start + offset),
            _ => Err(Error::OutOfBounds),
        }
    }
}

impl ErrorType for Partition {
    type Error = Error;
}

impl ReadNorFlash for Partition {
    const READ_SIZE: usize = 1;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error> {
        let addr = self.check(offset, bytes.len())?;
        if bytes.is_empty() {
            return Ok(());
        }
        flash_op(OP_READ, addr, bytes.as_mut_ptr(), bytes.len())
    }

    fn capacity(&self) -> usize {
        self.size as usize
    }
}

impl NorFlash for Partition {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = SECTOR as usize;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Error> {
        if from > to {
            return Err(Error::OutOfBounds);
        }
        let addr = self.check(from, (to - from) as usize)?;
        if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) {
            return Err(Error::NotAligned);
        }
        if from == to {
            return Ok(());
        }
        flash_op(OP_ERASE, addr, core::ptr::null_mut(), (to - from) as usize)
    }

    /// `flash_op` programs whole pages; the bytes around `bytes` are written as 0xFF,
    /// which leaves them unchanged.
    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Error> {
        let mut addr = self.check(offset, bytes.len())?;
        let mut rest = bytes;
        let mut page = [0xFF; PAGE];
        while !rest.is_empty() {
            let base = addr & !(PAGE as u32 - 1);
            let at = (addr - base) as usize;
            let n = rest.len().min(PAGE - at);
            page.fill(0xFF);
            page[at..at + n].copy_from_slice(&rest[..n]);
            flash_op(OP_PROGRAM, base, page.as_mut_ptr(), PAGE)?;
            addr += n as u32;
            rest = &rest[n..];
        }
        Ok(())
    }
}

impl MultiwriteNorFlash for Partition {}
