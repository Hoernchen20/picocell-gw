//! Provisioned configuration (sketch.md section 7) in the config partition, flash
//! offset 0x1F0000 (64 KB): `sequential-storage` map items encrypted with AES-128-GCM,
//! one holding the settings, one the sequence number of the last CA update (sketch.md
//! section 8.1, 8e; kept by `commit`, so a re-provisioned board still refuses older
//! updates) and, during a remote password change (sketch.md section 5.2), one holding
//! the new password next to them until the broker has accepted it.
//!
//! The key comes from `config_key`: in dev builds derived from the chip ID, which
//! anyone holding the board can read (it is also the gateway ID), so that only keeps
//! the secrets out of a plain flash dump; in production builds from a random key in
//! OTP (sketch.md section 4.2). Without a key nothing can be stored.
//!
//! The partition lies behind the two 988 KB slots, so flashing firmware keeps it. All
//! access goes through ROM `flash_op` (`crate::flash`): with the A/B partition table XIP
//! only maps the running slot.

use core::cell::{Cell, RefCell};
use core::ops::Range;
use core::str::FromStr;

use embassy_crypto::Aes128Gcm;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::once_lock::OnceLock;
use log::{info, warn};
use sequential_storage::cache::{Cache, Uncached};
use sequential_storage::map::{MapConfig, MapStorage};
use heapless::String;
use settings::ENCODED_MAX;
use static_cell::StaticCell;

pub use settings::{Net, Settings, check_password};

use crate::flash::Partition;
use crate::{ca_update, config_key, tls};

/// Unix time of the build; SNTP results before it are rejected.
const BUILD_TIME: &str = env!("PICOCELL_BUILD_TIME");

pub fn build_time() -> u64 {
    u64::from_str(BUILD_TIME).unwrap_or(0)
}

/// Physical flash range of the config partition (`fw/partition-table.json`).
const PARTITION: Range<u32> = 0x1F_0000..0x20_0000;
/// Map item keys.
const SETTINGS: u8 = 1;
const NEW_PASSWORD: u8 = 2;
const CA_SEQ: u8 = 3;
const NONCE: usize = 12;
const TAG: usize = 16;
/// The item: format byte, nonce, encrypted settings, tag.
const RECORD_MAX: usize = 1 + NONCE + ENCODED_MAX + TAG;
const RECORD_FORMAT: u8 = 1;

/// Binds each record to its item.
fn aad(item: u8) -> &'static [u8] {
    match item {
        SETTINGS => b"picocell-gw config",
        CA_SEQ => b"picocell-gw ca sequence",
        _ => b"picocell-gw new password",
    }
}

pub type Password = String<64>;

type Map = MapStorage<u8, Partition, Cache<Uncached, Uncached, Uncached, u8>>;

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
static STAGED: BlockingMutex<CriticalSectionRawMutex, RefCell<Option<Password>>> =
    BlockingMutex::new(RefCell::new(None));
static SEQ: BlockingMutex<CriticalSectionRawMutex, Cell<u64>> = BlockingMutex::new(Cell::new(0));

fn storage_error<E>(e: sequential_storage::Error<E>) -> &'static str {
    match e {
        sequential_storage::Error::FullStorage => "config partition full",
        sequential_storage::Error::Corrupted { .. } => "config partition corrupted, \"erase\" resets it",
        _ => "flash error",
    }
}

impl Store {
    /// Decrypts `item` into `plain`; its length, or `None` if not stored.
    async fn load(&mut self, item: u8) -> Result<Option<usize>, &'static str> {
        let record: Option<&[u8]> = self.map.fetch_item(&mut self.scratch, &item).await.map_err(storage_error)?;
        let Some(record) = record else {
            return Ok(None);
        };
        let &[RECORD_FORMAT, ref rest @ ..] = record else {
            return Err("unknown record format");
        };
        let Some(len) = rest.len().checked_sub(NONCE + TAG).filter(|n| *n <= ENCODED_MAX) else {
            return Err("bad record length");
        };
        let (nonce, rest) = rest.split_at(NONCE);
        let (data, tag) = rest.split_at(len);
        let plain = &mut self.plain[..len];
        self.aes
            .decrypt_to(nonce.try_into().unwrap(), aad(item), data, plain, tag.try_into().unwrap())
            .map_err(|_| "decryption failed (other board or key?)")?;
        Ok(Some(len))
    }

    /// Encrypts the first `n` bytes of `plain` as `item`.
    async fn store(&mut self, item: u8, n: usize) -> Result<(), &'static str> {
        let mut nonce = [0u8; NONCE];
        tls::random_bytes(&mut nonce);
        self.record[0] = RECORD_FORMAT;
        self.record[1..1 + NONCE].copy_from_slice(&nonce);
        let data = &mut self.record[1 + NONCE..1 + NONCE + n];
        data.copy_from_slice(&self.plain[..n]);
        let tag = self.aes.encrypt(&nonce, aad(item), data).map_err(|_| "encryption failed")?;
        self.record[1 + NONCE + n..1 + NONCE + n + TAG].copy_from_slice(&tag);
        let record: &[u8] = &self.record[..1 + NONCE + n + TAG];
        self.map.store_item(&mut self.scratch, &item, &record).await.map_err(storage_error)
    }

    async fn load_settings(&mut self) -> Result<Option<Settings>, &'static str> {
        match self.load(SETTINGS).await? {
            Some(n) => Settings::decode(&self.plain[..n]).map(Some),
            None => Ok(None),
        }
    }

    async fn save_settings(&mut self, s: &Settings) -> Result<(), &'static str> {
        let n = s.encode(&mut self.plain).len();
        self.store(SETTINGS, n).await?;
        // Read it back before reporting success.
        match self.load_settings().await? {
            Some(back) if back == *s => Ok(()),
            _ => Err("read-back differs"),
        }
    }

    async fn load_password(&mut self) -> Result<Option<Password>, &'static str> {
        let Some(n) = self.load(NEW_PASSWORD).await? else {
            return Ok(None);
        };
        let pw = core::str::from_utf8(&self.plain[..n]).map_err(|_| "not text")?;
        check_password(pw)?;
        Ok(Some(Password::try_from(pw).unwrap()))
    }

    /// 0 if no CA update was stored yet.
    async fn load_seq(&mut self) -> Result<u64, &'static str> {
        match self.load(CA_SEQ).await? {
            Some(8) => Ok(u64::from_le_bytes(self.plain[..8].try_into().unwrap())),
            Some(_) => Err("bad length"),
            None => Ok(0),
        }
    }

    async fn save_seq(&mut self, seq: u64) -> Result<(), &'static str> {
        self.plain[..8].copy_from_slice(&seq.to_le_bytes());
        self.store(CA_SEQ, 8).await?;
        if self.load_seq().await? != seq {
            return Err("read-back differs");
        }
        SEQ.lock(|s| s.set(seq));
        Ok(())
    }

    /// The sequence number first: if the settings write fails after it, the update
    /// is lost and has to be signed again, but an older one is still refused.
    async fn save_ca(&mut self, seq: u64, s: &Settings) -> Result<(), &'static str> {
        self.save_seq(seq).await?;
        self.save_settings(s).await
    }

    async fn drop_password(&mut self) -> Result<(), &'static str> {
        STAGED.lock(|s| s.take());
        self.map.remove_item(&mut self.scratch, &NEW_PASSWORD).await.map_err(storage_error)
    }
}

/// The store is only opened with a config key.
const NO_STORE: &str = "no config key, see \"otp-key\"";

/// Opens the config partition and loads the settings; runs before anything calls
/// `get()`. Without a complete config the gateway stays in provisioning mode.
pub async fn init() {
    let Some(key) = config_key::load() else {
        warn!("provisioning mode: no config key in OTP, see \"otp-key\" on the USB console");
        let _ = ACTIVE.init(Settings::new());
        return;
    };
    static CELL: StaticCell<Store> = StaticCell::new();
    let store = CELL.init(Store {
        map: MapStorage::new(
            Partition::new(PARTITION),
            const { MapConfig::new(0..PARTITION.end - PARTITION.start) },
            Cache::new_uncached(),
        ),
        aes: Aes128Gcm::new(&key),
        record: [0; RECORD_MAX],
        plain: [0; ENCODED_MAX],
        scratch: [0; RECORD_MAX + 8],
    });
    match store.load_seq().await {
        Ok(seq) => SEQ.lock(|s| s.set(seq)),
        Err(e) => {
            warn!("CA sequence number unusable ({e}): CA updates refused until \"erase\"");
            SEQ.lock(|s| s.set(u64::MAX));
        }
    }
    let (mut settings, stored) = match store.load_settings().await {
        Ok(Some(s)) => (s, true),
        Ok(None) => {
            warn!("no config stored");
            (Settings::new(), false)
        }
        Err(e) => {
            warn!("stored config unusable: {e}");
            (Settings::new(), false)
        }
    };
    if stored {
        match install_image_ca(store, &mut settings).await {
            Ok(true) => info!("CA certificate of this image stored in tls.ca1"),
            Ok(false) => {}
            Err(e) => warn!("storing the CA certificate of this image: {e}"),
        }
    }
    match settings.missing() {
        None => info!("config loaded"),
        Some(k) => warn!("provisioning mode: {k} not set; configure on the USB console, see \"help\""),
    }
    match store.load_password().await {
        Ok(Some(pw)) => {
            info!("new password staged, trying it first");
            STAGED.lock(|s| s.replace(Some(pw)));
        }
        Ok(None) => {}
        Err(e) => warn!("staged password unusable: {e}"),
    }
    let _ = ACTIVE.init(settings);
    *STORE.lock().await = Some(store);
}

/// A CA certificate built into the image (`PICOCELL_CA`) goes into `tls.ca1` once:
/// when it is in neither slot and the image is newer than the last CA update (the
/// build time is its sequence number), so an image cannot bring back a CA that a
/// later update replaced. Changes `s` in place (no second copy in the main task's
/// future): if storing fails, the CA is still used until the restart.
async fn install_image_ca(store: &mut Store, s: &mut Settings) -> Result<bool, &'static str> {
    let ca = ca_update::IMAGE_CA;
    let seq = build_time();
    if ca.is_empty() || s.ca(0) == ca || s.ca(1) == ca || seq <= ca_seq() {
        return Ok(false);
    }
    s.set_ca(1, ca)?;
    store.save_ca(seq, s).await?;
    Ok(true)
}

/// The settings in use: the stored ones (possibly incomplete), or none.
pub fn get() -> &'static Settings {
    ACTIVE.try_get().expect("config not loaded")
}

/// A complete config was loaded, so the network side runs.
pub fn provisioned() -> bool {
    get().missing().is_none()
}

/// Stores `s`; takes effect after a restart. Supersedes a staged password.
pub async fn save(s: &Settings) -> Result<(), &'static str> {
    let mut store = STORE.lock().await;
    let store = store.as_mut().ok_or(NO_STORE)?;
    store.save_settings(s).await?;
    store.drop_password().await
}

/// The new password of a remote change while the broker has not accepted it yet.
pub fn staged_password() -> Option<Password> {
    STAGED.lock(|s| s.borrow().clone())
}

/// Stores a new password next to the settings (first phase of a remote change).
pub async fn stage_password(pw: &str) -> Result<(), &'static str> {
    check_password(pw)?;
    let mut store = STORE.lock().await;
    let store = store.as_mut().ok_or(NO_STORE)?;
    store.plain[..pw.len()].copy_from_slice(pw.as_bytes());
    store.store(NEW_PASSWORD, pw.len()).await?;
    match store.load_password().await? {
        Some(back) if back == pw => {}
        _ => return Err("read-back differs"),
    }
    STAGED.lock(|s| s.replace(Some(Password::try_from(pw).unwrap())));
    Ok(())
}

/// The broker accepted the staged password: stores the settings with it and drops the
/// staged one. Takes effect after a restart.
pub async fn commit_password() -> Result<(), &'static str> {
    let pw = staged_password().ok_or("no password staged")?;
    let mut s = get().clone();
    s.set("mqtt.password", &pw)?;
    let mut store = STORE.lock().await;
    let store = store.as_mut().ok_or(NO_STORE)?;
    store.save_settings(&s).await?;
    store.drop_password().await
}

/// The broker refused the staged password: drops it, the stored one stays.
pub async fn drop_password() -> Result<(), &'static str> {
    STORE.lock().await.as_mut().ok_or(NO_STORE)?.drop_password().await
}

/// Sequence number of the last CA update (`ca_update`), 0 if none.
pub fn ca_seq() -> u64 {
    SEQ.lock(|s| s.get())
}

/// Stores a signed CA update: `der` into `tls.ca1` (empty: clears it) with its
/// sequence number. Takes effect after a restart.
pub async fn store_ca(seq: u64, der: &[u8]) -> Result<(), &'static str> {
    let mut s = get().clone();
    if der.is_empty() {
        s.unset(settings::CA_KEYS[1])?;
    } else {
        s.set_ca(1, der)?;
    }
    STORE.lock().await.as_mut().ok_or(NO_STORE)?.save_ca(seq, &s).await
}

/// The broker is only reachable with `tls.ca1`: it replaces `tls.ca0`, and `tls.ca1`
/// is cleared, so the old CA is no longer trusted. Takes effect after a restart.
pub async fn promote_ca() -> Result<(), &'static str> {
    let mut s = get().clone();
    s.promote_ca()?;
    STORE.lock().await.as_mut().ok_or(NO_STORE)?.save_settings(&s).await
}

/// Erases the config partition.
pub async fn erase() -> Result<(), &'static str> {
    STORE.lock().await.as_mut().ok_or(NO_STORE)?.map.erase_all().await.map_err(storage_error)
}
