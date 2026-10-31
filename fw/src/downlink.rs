//! Downlinks (sketch.md section 3.3): `DownlinkFrame` → duty-cycle check → JIT queue →
//! radio, answered with a `DownlinkTxAck`.
//!
//! The counter is the SX1302's (`radio::count_us`), the RP2350 timer while the
//! concentrator is not running. The TX task hands each frame to the radio
//! (`radio::transmit`); while the concentrator is not running it logs the frame at its
//! start time instead.
//!
//! On USB power (TPS2116 ST low on GP27, sketch.md section 2.4) frames go out at no more
//! than `USB_MAX_DBM`; the pull-down makes a board without the connector PCB count as
//! on USB.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use chirpstack::gw::TxAckStatus;
use chirpstack::Timing;
use critical_section::Mutex;
use embassy_futures::select::{Either, select};
use embassy_rp::gpio::Input;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};
use heapless::Vec;
use jit::duty::{self, EU868};
use jit::{At, Queue};
use log::{info, warn};

use crate::radio::{self, TxError};

/// TX power cap while the board runs on USB VBUS (500 mA budget); as on the desk.
const USB_MAX_DBM: i32 = 14;

/// A frame waiting for its start time.
struct Tx {
    downlink_id: u32,
    airtime_us: u32,
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

/// Queue and duty cycle under one lock: a frame is checked, queued and charged at once.
struct Scheduler {
    queue: Queue<Tx, 8>,
    duty: duty::Tracker<{ EU868.len() }>,
}

static SCHEDULER: Mutex<RefCell<Scheduler>> =
    Mutex::new(RefCell::new(Scheduler { queue: Queue::new(), duty: duty::Tracker::new() }));

enum Rejected {
    DutyCycle { used_us: u64 },
    Queue(jit::Error),
}
/// A frame was queued: the TX task re-checks which one comes first.
static QUEUED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

static TX_RECEIVED: AtomicU32 = AtomicU32::new(0);
static TX_EMITTED: AtomicU32 = AtomicU32::new(0);

/// The 1 MHz counter that uplink contexts carry and delay timing refers to. It jumps
/// when the concentrator starts or stops; frames queued then may miss their start.
pub fn count_us() -> u32 {
    radio::count_us().unwrap_or(Instant::now().as_micros() as u32)
}

/// Monotonic µs for the duty-cycle window, unlike `count_us` which wraps.
fn now_us() -> u64 {
    Instant::now().as_micros()
}

/// Downlink frames received and frames sent since the previous call, for `GatewayStats`.
pub fn take_counters() -> (u32, u32) {
    (TX_RECEIVED.swap(0, Ordering::Relaxed), TX_EMITTED.swap(0, Ordering::Relaxed))
}

/// Airtime counted against each EU868 sub-band, for `GatewayStats`.
pub fn duty_cycle_used() -> [u64; EU868.len()] {
    let now = now_us();
    critical_section::with(|cs| {
        let s = SCHEDULER.borrow_ref(cs);
        core::array::from_fn(|b| s.duty.used_us(b, now))
    })
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
    let Some(band) = duty::band(&EU868, tx.frequency, tx.bandwidth) else {
        warn!("downlink {id}/{i}: {} Hz BW{} not within a sub-band", tx.frequency, tx.bandwidth / 1000);
        return TxAckStatus::TxFreq;
    };
    if tx.power > EU868[band].max_eirp_dbm {
        warn!("downlink {id}/{i}: {} dBm too high for {} MHz", tx.power, EU868[band].name);
        return TxAckStatus::TxPower;
    }
    // The HAL would send below the LUT at its lowest entry, above the requested power.
    let (min, max) = sx1302::Conf::EU868.tx_power_range();
    if !(i32::from(min)..=i32::from(max)).contains(&tx.power) {
        warn!("downlink {id}/{i}: {} dBm outside the TX gain table ({min}..{max} dBm)", tx.power);
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
        warn!(
            "downlink {id}/{i}: SF{} BW{} preamble {} unsupported",
            tx.spreading_factor, tx.bandwidth, tx.preamble
        );
        return TxAckStatus::InternalError;
    };
    // LoRa carries at most 255 bytes; `DownlinkFrameItem.phy_payload` holds 256.
    let payload = match Vec::from_slice(tx.payload) {
        Ok(p) if p.len() <= 255 => p,
        _ => {
            warn!("downlink {id}/{i}: {} B payload too long", tx.payload.len());
            return TxAckStatus::InternalError;
        }
    };
    let entry = Tx {
        downlink_id: id,
        airtime_us: airtime,
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
    let (now, mono) = (count_us(), now_us());
    let r = critical_section::with(|cs| {
        let s = &mut *SCHEDULER.borrow_ref_mut(cs);
        let used = s.duty.used_us(band, mono);
        if !s.duty.allows(&EU868, band, mono, airtime) {
            return Err(Rejected::DutyCycle { used_us: used });
        }
        let start = s.queue.enqueue(now, at, airtime, entry).map_err(Rejected::Queue)?;
        // Charged when queued; a frame that later misses its start stays charged.
        s.duty.charge(band, mono.wrapping_add_signed(jit::offset(start, now).into()), airtime);
        Ok((start, used + u64::from(airtime)))
    });
    match r {
        Ok((start, used)) => {
            QUEUED.signal(());
            info!(
                "downlink {id}/{i}: queued for count_us {start} (in {} ms, {} ms airtime, {} MHz {} of {} ms/h)",
                jit::offset(start, now) / 1000,
                airtime / 1000,
                EU868[band].name,
                used / 1000,
                EU868[band].limit_us() / 1000
            );
            TxAckStatus::Ok
        }
        Err(Rejected::DutyCycle { used_us }) => {
            warn!(
                "downlink {id}/{i}: duty cycle: {} ms airtime, {} MHz {} of {} ms/h used",
                airtime / 1000,
                EU868[band].name,
                used_us / 1000,
                EU868[band].limit_us() / 1000
            );
            TxAckStatus::DutyCycleOverflow
        }
        Err(Rejected::Queue(e)) => {
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

/// The frame for the HAL, starting at `start`, at `power` dBm.
fn radio_tx(start: u32, tx: &Tx, power: i32) -> sx1302::Tx {
    let mut r = sx1302::Tx::EMPTY;
    r.freq_hz = tx.frequency;
    r.count_us = start;
    r.bandwidth_hz = tx.bandwidth;
    // Checked by `lora_airtime_us`: SF7-12, a 16-bit preamble.
    r.sf = tx.spreading_factor as u8;
    r.preamble = tx.preamble as u16;
    r.coderate = tx.code_rate as u8;
    // Within the gain table, checked by `schedule`; the USB cap is in it too.
    r.rf_power = power as i8;
    r.invert_pol = tx.invert_polarity;
    r.crc = tx.crc;
    // At most 255 bytes, checked by `schedule`.
    r.set_payload(&tx.payload);
    r
}

/// Takes each frame from the queue `jit::HANDOVER_US` before its start. `on_vbus` is the
/// TPS2116 ST input (GP27, low on USB).
#[embassy_executor::task]
pub async fn tx_task(on_vbus: Input<'static>) -> ! {
    loop {
        let Some(start) = critical_section::with(|cs| SCHEDULER.borrow_ref(cs).queue.next_start()) else {
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
        let Some((start, tx)) = critical_section::with(|cs| SCHEDULER.borrow_ref_mut(cs).queue.pop_due(count_us()))
        else {
            continue;
        };
        let lead = jit::offset(start, count_us());
        if lead < jit::TX_START_DELAY_US as i32 {
            warn!("downlink {}: missed its start by {} µs", tx.downlink_id, -lead);
            critical_section::with(|cs| SCHEDULER.borrow_ref_mut(cs).queue.tx_done());
            continue;
        }
        // Read per frame: the 24 V input can come and go.
        let power = if on_vbus.is_low() { tx.power.min(USB_MAX_DBM) } else { tx.power };
        let sent = match radio::transmit(radio_tx(start, &tx, power), tx.airtime_us).await {
            Ok(()) => Ok("TX"),
            // Not running when the frame was queued either: the timer stood in for its
            // counter, so the start is still right.
            Err(TxError::NotRunning) if radio::count_us().is_none() => {
                let lead = jit::offset(start, count_us());
                if lead > 0 {
                    Timer::after_micros(lead as u64).await;
                }
                Timer::after_micros(u64::from(tx.airtime_us)).await;
                Ok("TX (no radio)")
            }
            Err(e) => Err(e),
        };
        // Until here the queue kept the frame's airtime blocked.
        critical_section::with(|cs| SCHEDULER.borrow_ref_mut(cs).queue.tx_done());
        match sent {
            Ok(what) => {
                TX_EMITTED.fetch_add(1, Ordering::Relaxed);
                info!(
                    "downlink {}: {what} at count_us {start}: {} Hz, {power} dBm{}, SF{} BW{} CR4/{}{}, preamble {}{}, {} B",
                    tx.downlink_id,
                    tx.frequency,
                    if power < tx.power { " (USB power)" } else { "" },
                    tx.spreading_factor,
                    tx.bandwidth / 1000,
                    tx.code_rate + 4,
                    if tx.invert_polarity { ", inverted" } else { "" },
                    tx.preamble,
                    if tx.crc { ", CRC" } else { "" },
                    tx.payload.len()
                );
            }
            Err(TxError::Late { lead_us }) => {
                warn!("downlink {}: not sent, {lead_us} µs before its start", tx.downlink_id)
            }
            Err(TxError::NotRunning) => warn!("downlink {}: not sent, radio restarted", tx.downlink_id),
            Err(TxError::Failed(why)) => warn!("downlink {}: not sent, {why}", tx.downlink_id),
        }
    }
}
