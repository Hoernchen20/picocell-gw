//! Downlinks (sketch.md section 3.3): `DownlinkFrame` → JIT queue → radio, answered
//! with a `DownlinkTxAck`.
//!
//! No radio yet: the counter is the RP2350 timer instead of the SX1302's, and the TX
//! task logs each frame at its start time instead of sending it.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use chirpstack::gw::TxAckStatus;
use chirpstack::Timing;
use critical_section::Mutex;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};
use heapless::Vec;
use jit::{At, Queue};
use log::{info, warn};

/// EU868 band edges.
const FREQUENCY_HZ: core::ops::RangeInclusive<u32> = 863_000_000..=870_000_000;
/// EU868 maximum (869.4–869.65 MHz); per sub-band limits come with duty-cycle tracking.
const MAX_POWER_DBM: i32 = 27;

/// A frame waiting for its start time.
struct Tx {
    downlink_id: u32,
    frequency: u32,
    power: i32,
    bandwidth: u32,
    spreading_factor: u32,
    code_rate: u32,
    invert_polarity: bool,
    preamble: u32,
    crc: bool,
    payload: Vec<u8, 256>,
}

static QUEUE: Mutex<RefCell<Queue<Tx, 8>>> = Mutex::new(RefCell::new(Queue::new()));
/// A frame was queued: the TX task re-checks which one comes first.
static QUEUED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

static TX_RECEIVED: AtomicU32 = AtomicU32::new(0);
static TX_EMITTED: AtomicU32 = AtomicU32::new(0);

/// The 1 MHz counter that uplink contexts carry and delay timing refers to.
pub fn count_us() -> u32 {
    Instant::now().as_micros() as u32
}

/// Downlink frames received and frames sent since the previous call, for `GatewayStats`.
pub fn take_counters() -> (u32, u32) {
    (TX_RECEIVED.swap(0, Ordering::Relaxed), TX_EMITTED.swap(0, Ordering::Relaxed))
}

/// Queues the first item of a `DownlinkFrame` that can be sent and encodes the
/// `DownlinkTxAck` into `ack`. `None` if the frame is not valid protobuf.
pub fn handle(gateway_id: &str, payload: &[u8], ack: &mut [u8]) -> Option<usize> {
    let frame: chirpstack::gw::DownlinkFrame = match chirpstack::decode(payload) {
        Ok(f) => f,
        Err(_) => {
            warn!("downlink: malformed DownlinkFrame ({} B)", payload.len());
            return None;
        }
    };
    TX_RECEIVED.fetch_add(1, Ordering::Relaxed);
    let id = frame.downlink_id;
    // At most one item is sent; the ones after it are ignored.
    let mut statuses: Vec<TxAckStatus, 4> = Vec::new();
    let mut queued = false;
    for (i, item) in frame.items.iter().enumerate() {
        let status = if queued { TxAckStatus::Ignored } else { schedule(id, i, item) };
        queued |= status == TxAckStatus::Ok;
        let _ = statuses.push(status);
    }
    chirpstack::tx_ack(gateway_id, id, &statuses, ack).ok()
}

fn schedule(id: u32, i: usize, item: &chirpstack::gw::DownlinkFrameItem) -> TxAckStatus {
    let tx = match chirpstack::lora_tx(item) {
        Ok(tx) => tx,
        Err(e) => {
            warn!("downlink {id}/{i}: unsupported: {e:?}");
            return TxAckStatus::InternalError;
        }
    };
    if !FREQUENCY_HZ.contains(&tx.frequency) {
        warn!("downlink {id}/{i}: {} Hz outside the band", tx.frequency);
        return TxAckStatus::TxFreq;
    }
    if tx.power > MAX_POWER_DBM {
        warn!("downlink {id}/{i}: {} dBm too high", tx.power);
        return TxAckStatus::TxPower;
    }
    let at = match tx.timing {
        Timing::Delay { count_us, delay_us } => At::Count(count_us.wrapping_add(delay_us)),
        Timing::Immediately => At::Asap,
        // No GPS fitted (sketch.md section 3.3).
        Timing::GpsEpoch => return TxAckStatus::GpsUnlocked,
    };
    let airtime = jit::lora_airtime_us(
        tx.bandwidth,
        tx.spreading_factor,
        tx.code_rate,
        tx.preamble,
        tx.crc,
        tx.payload.len(),
    );
    let Some(airtime) = airtime else {
        warn!("downlink {id}/{i}: SF{} BW{} unsupported", tx.spreading_factor, tx.bandwidth);
        return TxAckStatus::InternalError;
    };
    let Ok(payload) = Vec::from_slice(tx.payload) else {
        // Cannot happen: same capacity as DownlinkFrameItem.phy_payload.
        warn!("downlink {id}/{i}: {} B payload too long", tx.payload.len());
        return TxAckStatus::InternalError;
    };
    let entry = Tx {
        downlink_id: id,
        frequency: tx.frequency,
        power: tx.power,
        bandwidth: tx.bandwidth,
        spreading_factor: tx.spreading_factor,
        code_rate: tx.code_rate,
        invert_polarity: tx.invert_polarity,
        preamble: tx.preamble,
        crc: tx.crc,
        payload,
    };
    let now = count_us();
    let r = critical_section::with(|cs| QUEUE.borrow_ref_mut(cs).enqueue(now, at, airtime, entry));
    match r {
        Ok(start) => {
            QUEUED.signal(());
            info!(
                "downlink {id}/{i}: queued for count_us {start} (in {} ms, {} ms airtime)",
                jit::offset(start, now) / 1000,
                airtime / 1000
            );
            TxAckStatus::Ok
        }
        Err(e) => {
            if let At::Count(t) = at {
                warn!("downlink {id}/{i}: {e:?} (in {} ms)", jit::offset(t, now) / 1000);
            } else {
                warn!("downlink {id}/{i}: {e:?}");
            }
            match e {
                jit::Error::TooLate => TxAckStatus::TooLate,
                jit::Error::TooEarly => TxAckStatus::TooEarly,
                jit::Error::Collision => TxAckStatus::CollisionPacket,
                jit::Error::Full => TxAckStatus::QueueFull,
            }
        }
    }
}

/// Takes each frame from the queue `jit::HANDOVER_US` before its start.
#[embassy_executor::task]
pub async fn tx_task() -> ! {
    loop {
        let Some(start) = critical_section::with(|cs| QUEUE.borrow_ref(cs).next_start()) else {
            QUEUED.wait().await;
            continue;
        };
        let wait = jit::offset(start, count_us()) - jit::HANDOVER_US as i32;
        if wait > 0 {
            // A frame queued in the meantime may start earlier.
            if let Either::Second(()) = select(Timer::after_micros(wait as u64), QUEUED.wait()).await {
                continue;
            }
        }
        let Some((start, tx)) = critical_section::with(|cs| QUEUE.borrow_ref_mut(cs).pop_due(count_us()))
        else {
            continue;
        };
        // The radio takes the frame here, timestamped with `start` (lgw_send). Without
        // one, wait for the start and log it.
        let lead = jit::offset(start, count_us());
        if lead < jit::TX_START_DELAY_US as i32 {
            warn!("downlink {}: missed its start by {} µs", tx.downlink_id, -lead);
            continue;
        }
        Timer::after_micros(lead as u64).await;
        TX_EMITTED.fetch_add(1, Ordering::Relaxed);
        info!(
            "downlink {}: TX (no radio) at count_us {start}: {} Hz, {} dBm, SF{} BW{} CR4/{}{}, preamble {}{}, {} B",
            tx.downlink_id,
            tx.frequency,
            tx.power,
            tx.spreading_factor,
            tx.bandwidth / 1000,
            tx.code_rate + 4,
            if tx.invert_polarity { ", inverted" } else { "" },
            tx.preamble,
            if tx.crc { ", CRC" } else { "" },
            tx.payload.len()
        );
    }
}
