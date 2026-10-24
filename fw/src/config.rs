//! Provisioned configuration (sketch.md section 7) in the config partition, flash
//! offset 0x1F0000 (64 KB): one `sequential-storage` map item holding the settings,
//! encrypted with AES-128-GCM.
//!
//! The key is a development key derived from the chip ID, which anyone holding the
//! board can read (it is also the gateway ID), so for now this only keeps the secrets
//! out of a plain flash dump. Step 8 replaces it with a random key in OTP (sketch.md
//! section 4.2); configs stored before then have to be provisioned again.
//!
//! The partition lies behind the 988 KB image, so flashing firmware keeps it. Without a
//! partition table XIP maps the whole flash, so reads reach the partition directly;
//! with the A/B table (step 8) they need ROM `flash_op` (sketch.md section 3.1).

use core::ops::Range;
use core::str::FromStr;

use embassy_crypto::{Aes128Gcm, HmacSha256};
use embassy_rp::flash::{Async, Flash};
use embassy_rp::peripherals::FLASH;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::once_lock::OnceLock;
use log::{info, warn};
use sequential_storage::cache::{Cache, Uncached};
use sequential_storage::map::{MapConfig, MapStorage};
use settings::ENCODED_MAX;
use static_cell::StaticCell;

pub use settings::{Net, Settings};

use crate::tls;

/// Unix time of the build; SNTP results before it are rejected.
const BUILD_TIME: &str = env!("PICOCELL_BUILD_TIME");

pub fn build_time() -> u64 {
    u64::from_str(BUILD_TIME).unwrap_or(0)
}

pub const FLASH_SIZE: usize = 2 * 1024 * 1024;
pub type ConfigFlash = Flash<'static, FLASH, Async, FLASH_SIZE>;

const PARTITION: Range<u32> = 0x1F_0000..0x20_0000;
/// Key of the one map item.
const ITEM: u8 = 1;
const NONCE: usize = 12;
const TAG: usize = 16;
/// The item: format byte, nonce, encrypted settings, tag.
const RECORD_MAX: usize = 1 + NONCE + ENCODED_MAX + TAG;
const RECORD_FORMAT: u8 = 1;
const AAD: &[u8] = b"picocell-gw config";

type Map = MapStorage<u8, ConfigFlash, Cache<Uncached, Uncached, Uncached, u8>>;

struct Store {
    map: Map,
    aes: Aes128Gcm,
    record: [u8; RECORD_MAX],
    plain: [u8; ENCODED_MAX],
    /// For `sequential-storage`: key and item, rounded up to flash words.
    scratch: [u8; RECORD_MAX + 8],
}

// The store in a `StaticCell` (.bss): an `Option<Store>` initialiser would end up in
// .data and so in the flash image.
static STORE: Mutex<CriticalSectionRawMutex, Option<&'static mut Store>> = Mutex::new(None);
static ACTIVE: OnceLock<Settings> = OnceLock::new();

fn storage_error<E>(e: sequential_storage::Error<E>) -> &'static str {
    match e {
        sequential_storage::Error::FullStorage => "config partition full",
        sequential_storage::Error::Corrupted { .. } => "config partition corrupted, \"erase\" resets it",
        _ => "flash error",
    }
}

impl Store {
    async fn load(&mut self) -> Result<Option<Settings>, &'static str> {
        let item: Option<&[u8]> = self.map.fetch_item(&mut self.scratch, &ITEM).await.map_err(storage_error)?;
        let Some(item) = item else {
            return Ok(None);
        };
        let &[RECORD_FORMAT, ref rest @ ..] = item else {
            return Err("unknown record format");
        };
        let Some(len) = rest.len().checked_sub(NONCE + TAG).filter(|n| *n <= ENCODED_MAX) else {
            return Err("bad record length");
        };
        let (nonce, rest) = rest.split_at(NONCE);
        let (data, tag) = rest.split_at(len);
        let plain = &mut self.plain[..len];
        self.aes
            .decrypt_to(nonce.try_into().unwrap(), AAD, data, plain, tag.try_into().unwrap())
            .map_err(|_| "decryption failed (other board or key?)")?;
        Settings::decode(plain).map(Some)
    }

    async fn save(&mut self, s: &Settings) -> Result<(), &'static str> {
        let plain = s.encode(&mut self.plain);
        let n = plain.len();
        let mut nonce = [0u8; NONCE];
        tls::random_bytes(&mut nonce);
        self.record[0] = RECORD_FORMAT;
        self.record[1..1 + NONCE].copy_from_slice(&nonce);
        let data = &mut self.record[1 + NONCE..1 + NONCE + n];
        data.copy_from_slice(plain);
        let tag = self.aes.encrypt(&nonce, AAD, data).map_err(|_| "encryption failed")?;
        self.record[1 + NONCE + n..1 + NONCE + n + TAG].copy_from_slice(&tag);
        let item: &[u8] = &self.record[..1 + NONCE + n + TAG];
        self.map.store_item(&mut self.scratch, &ITEM, &item).await.map_err(storage_error)?;
        // Read it back before reporting success.
        match self.load().await? {
            Some(back) if back == *s => Ok(()),
            _ => Err("read-back differs"),
        }
    }
}

/// Opens the config partition and loads the settings; runs before anything calls
/// `get()`. Without a complete config the gateway stays in provisioning mode.
pub async fn init(flash: ConfigFlash) {
    let chip_id = embassy_rp::otp::get_chipid().unwrap_or(0);
    let key = HmacSha256::mac(b"picocell-gw dev config key", &chip_id.to_be_bytes());
    static CELL: StaticCell<Store> = StaticCell::new();
    let store = CELL.init(Store {
        map: MapStorage::new(flash, const { MapConfig::new(PARTITION) }, Cache::new_uncached()),
        aes: Aes128Gcm::new(key[..16].try_into().unwrap()),
        record: [0; RECORD_MAX],
        plain: [0; ENCODED_MAX],
        scratch: [0; RECORD_MAX + 8],
    });
    let settings = match store.load().await {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!("no config stored");
            Settings::new()
        }
        Err(e) => {
            warn!("stored config unusable: {e}");
            Settings::new()
        }
    };
    match settings.missing() {
        None => info!("config loaded"),
        Some(k) => warn!("provisioning mode: {k} not set; configure on the USB console, see \"help\""),
    }
    let _ = ACTIVE.init(settings);
    *STORE.lock().await = Some(store);
}

/// The settings in use: the stored ones (possibly incomplete), or none.
pub fn get() -> &'static Settings {
    ACTIVE.try_get().expect("config not loaded")
}

/// A complete config was loaded, so the network side runs.
pub fn provisioned() -> bool {
    get().missing().is_none()
}

/// Stores `s`; takes effect after a restart.
pub async fn save(s: &Settings) -> Result<(), &'static str> {
    STORE.lock().await.as_mut().ok_or("no flash")?.save(s).await
}

/// Erases the config partition.
pub async fn erase() -> Result<(), &'static str> {
    STORE.lock().await.as_mut().ok_or("no flash")?.map.erase_all().await.map_err(storage_error)
}
