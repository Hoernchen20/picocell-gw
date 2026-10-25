//! Firmware update over MQTT (sketch.md section 8.1, milestone 8b).
//!
//! The image (the sealed binary from 0x10000000 on, as in the UF2) goes into the slot
//! that is not running, in messages on `command/ota`:
//!
//! - begin: `0x01`, image size (u32 LE), SHA-256 of the image (32 bytes);
//! - chunk: `0x02`, offset (u32 LE), 4096 bytes of the image (fewer for the last).
//!
//! Each one is answered on `state/ota`: `next <offset>` asks for the chunk at that
//! offset (a chunk at another offset is ignored, so the sender just resends after a
//! timeout), `error: ...` ends the transfer, `verified ...` comes before the reboot.
//! The slot's first sector is erased at the begin and written only once the rest is in
//! flash and the SHA-256 (first sector from RAM, the rest read back) matches, so a
//! partial or corrupt image never has a valid block loop. The reboot is a FLASH_UPDATE
//! boot into the new image, which runs as a trial until it reaches the broker (`boot`).

use core::fmt::Write;
use core::ops::Range;

use embassy_crypto::Sha256;
use embassy_futures::yield_now;
use embedded_storage_async::nor_flash::{NorFlash, ReadNorFlash};
use log::{info, warn};

use crate::boot;
use crate::flash::{self, Partition};

/// Slots A and B of fw/partition-table.json.
const SLOTS: [Range<u32>; 2] = [0x2000..0xF_9000, 0xF_9000..0x1F_0000];
const NAMES: [&str; 2] = ["A", "B"];
const CHUNK: usize = flash::SECTOR as usize;
const BEGIN: u8 = 1;
const DATA: u8 = 2;

pub type Reply = heapless::String<64>;

struct Job {
    slot: usize,
    size: u32,
    sha256: [u8; 32],
    /// Offset of the next chunk; all before it are written (the first one in RAM).
    next: u32,
}

enum Step {
    Next(u32),
    Verified,
}

pub struct Receiver {
    job: Option<Job>,
    first: [u8; CHUNK],
}

impl Receiver {
    pub const fn new() -> Self {
        Self { job: None, first: [0; CHUNK] }
    }

    /// Handles a message on `command/ota`. Returns the answer for `state/ota` and
    /// whether to call `reboot()` once it is sent.
    pub async fn handle(&mut self, retain: bool, msg: &[u8]) -> (Reply, bool) {
        let mut r = Reply::new();
        // A retained one would come again on every connection; the transfer goes on.
        let step = if retain { Err("retained message refused") } else { self.step(msg).await };
        match step {
            Ok(Step::Next(n)) => {
                let _ = write!(r, "next {n}");
                (r, false)
            }
            Ok(Step::Verified) => {
                let _ = write!(r, "verified, rebooting into slot {}", NAMES[self.job.as_ref().unwrap().slot]);
                (r, true)
            }
            Err(e) => {
                warn!("ota: {e}");
                if !retain {
                    self.job = None;
                }
                let _ = write!(r, "error: {e}");
                (r, false)
            }
        }
    }

    /// Reboots into the verified image.
    pub fn reboot(&self) -> ! {
        boot::reboot_into(SLOTS[self.job.as_ref().unwrap().slot].start)
    }

    async fn step(&mut self, msg: &[u8]) -> Result<Step, &'static str> {
        match msg.split_first() {
            Some((&BEGIN, m)) => self.begin(m).await,
            Some((&DATA, m)) => self.data(m).await,
            _ => Err("unknown message type"),
        }
    }

    async fn begin(&mut self, m: &[u8]) -> Result<Step, &'static str> {
        self.job = None;
        if m.len() != 4 + 32 {
            return Err("begin: size and SHA-256 expected");
        }
        // The other slot still holds the image to fall back to.
        if boot::trial() {
            return Err("this image is still on trial");
        }
        let slot = match boot::info().map(|i| i.partition) {
            Some(0) => 1,
            Some(1) => 0,
            _ => return Err("not running from slot A or B"),
        };
        let size = u32::from_le_bytes(m[..4].try_into().unwrap());
        let range = SLOTS[slot].clone();
        if size == 0 || size > range.end - range.start {
            return Err("image size does not fit the slot");
        }
        Partition::new(range).erase(0, flash::SECTOR).await.map_err(|_| "flash erase failed")?;
        info!("ota: receiving {size} bytes into slot {}", NAMES[slot]);
        self.job = Some(Job { slot, size, sha256: m[4..].try_into().unwrap(), next: 0 });
        Ok(Step::Next(0))
    }

    async fn data(&mut self, m: &[u8]) -> Result<Step, &'static str> {
        let Self { job, first } = self;
        let Some(job) = job else {
            return Err("no transfer, begin first");
        };
        if m.len() < 4 {
            return Err("chunk: offset expected");
        }
        let offset = u32::from_le_bytes(m[..4].try_into().unwrap());
        let data = &m[4..];
        if offset != job.next {
            return Ok(Step::Next(job.next));
        }
        // Chunks are whole sectors, so `offset` starts one.
        let end = offset + data.len() as u32;
        if data.is_empty() || end > job.size || (data.len() != CHUNK && end != job.size) {
            return Err("chunk: 4096 bytes expected, fewer only for the last");
        }
        let mut slot = Partition::new(SLOTS[job.slot].clone());
        if offset == 0 {
            first[..data.len()].copy_from_slice(data);
        } else {
            slot.erase(offset, offset + flash::SECTOR).await.map_err(|_| "flash erase failed")?;
            slot.write(offset, data).await.map_err(|_| "flash write failed")?;
        }
        job.next = end;
        if end < job.size {
            return Ok(Step::Next(end));
        }

        let head = &first[..(job.size as usize).min(CHUNK)];
        let mut sha = Sha256::new();
        sha.update(head);
        let mut buf = [0u8; 1024];
        let mut at = CHUNK as u32;
        while at < job.size {
            let n = ((job.size - at) as usize).min(buf.len());
            slot.read(at, &mut buf[..n]).await.map_err(|_| "flash read failed")?;
            sha.update(&buf[..n]);
            at += n as u32;
            // Each read runs with interrupts off; let the other tasks in between.
            yield_now().await;
        }
        if sha.finalize() != job.sha256 {
            return Err("SHA-256 mismatch, image dropped");
        }
        slot.write(0, head).await.map_err(|_| "flash write failed")?;
        let step = buf.len();
        for (i, part) in head.chunks(step).enumerate() {
            let back = &mut buf[..part.len()];
            slot.read((i * step) as u32, back).await.map_err(|_| "flash read failed")?;
            if back != part {
                let _ = slot.erase(0, flash::SECTOR).await;
                return Err("first sector reads back wrong, image dropped");
            }
        }
        info!("ota: image verified in slot {}", NAMES[job.slot]);
        Ok(Step::Verified)
    }
}
