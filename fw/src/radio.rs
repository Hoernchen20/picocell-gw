//! WM1302 concentrator on core 1 (sketch.md section 3.2): power and reset, an SPI probe
//! of the SX1302, then sx1302_hal (`sx1302` crate) with the EU868 channel plan.
//! The HAL blocks (firmware upload, calibration, SPI polling), so it gets core 1 to
//! itself and core 0 keeps network and console. Core 1 hands frames with a valid CRC
//! to `uplink_task` on core 0 (`RECEIVED`), which publishes them as `UplinkFrame`s, and
//! keeps a snapshot of the SX1302 counter (`COUNTER`) for the downlink queue.
//! Downlinks go the other way: `transmit` leaves a frame for core 1 (`PENDING`), which
//! loads it into the SX1302 between polls (`lgw_send`, timestamped) and answers on
//! `TX_RESULT` once the TX modem is free again.
//!
//! Pins (sketch.md section 2.3): SPI1 GP10 SCK, GP11 MOSI, GP12 MISO, GP13 CS,
//! GP14 SX1302_RESET (active high), GP15 POWER_EN, I2C0 GP4 SDA / GP5 SCL to the
//! module's STTS751 temperature sensor, without which `lgw_start` fails. While the
//! module is off, the outputs stay low so nothing back-powers it through its I/O; the
//! I2C pull-ups are external (to the module's 3.3 V on the board).
//!
//! SPI framing as in sx1302_hal `loragw_spi.c`: a mux byte (0x00 = SX1302), the
//! 15-bit address with bit 7 of the first byte set for a write, then the data; a read
//! clocks one dummy byte before the data.

use core::cell::{Cell, RefCell};
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use critical_section::Mutex;
use embassy_rp::Peri;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::i2c::{self, I2c};
use embassy_rp::multicore::{Stack, spawn_core1};
use embassy_rp::peripherals::{CORE1, I2C0, PIN_4, PIN_5, PIN_10, PIN_11, PIN_12, PIN_13, PIN_14, PIN_15, SPI1};
use embassy_rp::spi::{self, Blocking, Spi};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, block_for};
use log::{info, warn};
use static_cell::ConstStaticCell;
use sx1302::{Bus, BusError, Concentrator, Conf, Crc, Data, Modulation, Packet, Tx, TxStatus};

use crate::clock;
use crate::mqtt::{UPLINKS, Uplink};

/// sx1302_hal `loragw_reg.c`: COMMON_PAGE_PAGE (bits 1:0, read/write).
const REG_PAGE: u16 = 0x5600;
/// COMMON_VERSION_VERSION (read-only).
const REG_VERSION: u16 = 0x5606;
const MUX_SX1302: u8 = 0x00;
/// The HAL's Linux default (`SPI_SPEED`); fine over jumper wires.
const SPI_HZ: u32 = 2_000_000;
/// As lora_pkt_fwd polls `lgw_receive`.
const POLL: Duration = Duration::from_millis(10);
/// Between polls core 1 checks this often for a frame to send and for the end of the one
/// on air.
const TX_STEP: Duration = Duration::from_micros(200);
/// A frame still on air this long after its computed end is stopped.
const TX_OVERRUN_US: i32 = 100_000;
/// Core 1 runs the HAL; `lgw_receive` and the calibration keep a few KB on the stack.
const CORE1_STACK: usize = 32 * 1024;

pub struct Radio {
    spi: Spi<'static, SPI1, Blocking>,
    i2c: I2c<'static, I2C0, i2c::Blocking>,
    cs: Output<'static>,
    reset: Output<'static>,
    power: Output<'static>,
}

/// What `probe` found.
#[derive(Clone, Copy)]
pub struct Probe {
    pub version: u8,
    /// The page register read back what was written (MOSI, MISO, SCK and CS work).
    pub loopback: bool,
    /// A reset pulse set the page register back to 0 (SX1302_RESET works).
    pub reset: bool,
}

impl Probe {
    /// A floating or shorted MISO reads as all zeros or all ones.
    pub fn answered(&self) -> bool {
        self.loopback && !matches!(self.version, 0x00 | 0xff)
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.version;
        write!(f, "version 0x{v:02x} (v{}.{}), ", v >> 4, v & 0x0f)?;
        write!(f, "page register {}", if self.loopback { "ok" } else { "failed" })?;
        if self.loopback {
            write!(f, ", reset {}", if self.reset { "ok" } else { "failed" })?;
        }
        Ok(())
    }
}

impl Radio {
    /// Takes the pins with the module off.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spi: Peri<'static, SPI1>,
        sck: Peri<'static, PIN_10>,
        mosi: Peri<'static, PIN_11>,
        miso: Peri<'static, PIN_12>,
        cs: Peri<'static, PIN_13>,
        reset: Peri<'static, PIN_14>,
        power: Peri<'static, PIN_15>,
        i2c: Peri<'static, I2C0>,
        sda: Peri<'static, PIN_4>,
        scl: Peri<'static, PIN_5>,
    ) -> Self {
        let mut spi_cfg = spi::Config::default();
        spi_cfg.frequency = SPI_HZ;
        let mut i2c_cfg = i2c::Config::default();
        i2c_cfg.sda_pullup = false;
        i2c_cfg.scl_pullup = false;
        Self {
            spi: Spi::new_blocking(spi, sck, mosi, miso, spi_cfg),
            i2c: I2c::new_blocking(i2c, scl, sda, i2c_cfg),
            cs: Output::new(cs, Level::Low),
            reset: Output::new(reset, Level::Low),
            power: Output::new(power, Level::Low),
        }
    }

    /// Switches the module off (CS low first) and on again, then resets the SX1302.
    fn power_cycle(&mut self) {
        self.cs.set_low();
        self.reset.set_low();
        self.power.set_low();
        block_for(Duration::from_millis(100));
        self.power.set_high();
        // Buck (or the Pi HAT regulator) up and settled.
        block_for(Duration::from_millis(100));
        self.cs.set_high();
        self.pulse_reset();
    }

    fn pulse_reset(&mut self) {
        self.reset.set_high();
        block_for(Duration::from_millis(10));
        self.reset.set_low();
        block_for(Duration::from_millis(10));
    }

    fn read(&mut self, addr: u16) -> u8 {
        let mut b = [MUX_SX1302, (addr >> 8) as u8 & 0x7f, addr as u8, 0, 0];
        self.cs.set_low();
        let _ = self.spi.blocking_transfer_in_place(&mut b);
        self.cs.set_high();
        b[4]
    }

    fn write(&mut self, addr: u16, data: u8) {
        let b = [MUX_SX1302, 0x80 | (addr >> 8) as u8 & 0x7f, addr as u8, data];
        self.cs.set_low();
        let _ = self.spi.blocking_write(&b);
        self.cs.set_high();
    }

    /// Reads the version, writes and reads back the page register, then checks that a
    /// reset pulse clears it.
    fn probe(&mut self) -> Probe {
        let version = self.read(REG_VERSION);
        let loopback = [1, 2, 3, 0, 2].into_iter().all(|p| {
            self.write(REG_PAGE, p);
            self.read(REG_PAGE) & 0x03 == p
        });
        self.pulse_reset();
        let reset = self.read(REG_PAGE) & 0x03 == 0;
        self.write(REG_PAGE, 0);
        Probe { version, loopback, reset }
    }
}

impl Bus for Radio {
    fn spi(&mut self, cmd: &[u8], data: Data<'_>) -> Result<(), BusError> {
        self.cs.set_low();
        let r = self.spi.blocking_write(cmd).and_then(|()| match data {
            Data::Write(d) => self.spi.blocking_write(d),
            Data::Exchange(d) => self.spi.blocking_transfer_in_place(d),
        });
        self.cs.set_high();
        r.map_err(|_| BusError)
    }

    fn i2c(&mut self, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), BusError> {
        match rd.is_empty() {
            true => self.i2c.blocking_write(addr, wr),
            false => self.i2c.blocking_write_read(addr, wr, rd),
        }
        .map_err(|_| BusError)
    }

    fn delay_us(&mut self, us: u32) {
        block_for(Duration::from_micros(us.into()));
    }

    fn now_us(&mut self) -> u64 {
        Instant::now().as_micros()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    /// The SX1302 did not answer on SPI (power, wiring).
    NoAnswer,
    /// The HAL call that failed.
    Failed(&'static str),
    Running,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Starting => f.write_str("starting"),
            State::NoAnswer => f.write_str("no answer from the SX1302, check power and wiring"),
            State::Failed(call) => write!(f, "{call} failed, see the log"),
            State::Running => f.write_str("running, EU868"),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Status {
    pub state: State,
    /// Bumped by each power-up.
    pub starts: u32,
    pub probe: Option<Probe>,
    pub eui: Option<u64>,
    /// Frames since the last start, with a valid CRC and with a CRC error.
    pub rx_ok: u32,
    pub rx_bad: u32,
    /// Frames sent since the last start, and those that were not.
    pub tx_ok: u32,
    pub tx_failed: u32,
    /// The longest `lgw_send` took since the last start, µs.
    pub tx_load_max_us: u32,
}

static STATUS: Mutex<Cell<Status>> = Mutex::new(Cell::new(Status {
    state: State::Starting,
    starts: 0,
    probe: None,
    eui: None,
    rx_ok: 0,
    rx_bad: 0,
    tx_ok: 0,
    tx_failed: 0,
    tx_load_max_us: 0,
}));
static RESTART: AtomicBool = AtomicBool::new(false);

/// A frame for `uplink_task`, with the time core 1 fetched it.
struct Received {
    packet: Packet,
    at: Instant,
}

/// Full only while nobody takes frames (offline); core 1 then drops them.
static RECEIVED: Channel<CriticalSectionRawMutex, Received, 8> = Channel::new();

/// A frame from `transmit`, taken by core 1.
struct Pending {
    tx: Tx,
    airtime_us: u32,
}

static PENDING: Mutex<RefCell<Option<Pending>>> = Mutex::new(RefCell::new(None));

/// What became of a frame from `transmit`.
#[derive(Clone, Copy, Debug)]
pub enum TxError {
    /// The concentrator is not running (any more).
    NotRunning,
    /// Too close to its start to load it, or loaded too late to make it.
    Late { lead_us: i32 },
    /// The HAL call that failed, or what went wrong on air.
    Failed(&'static str),
}

static TX_RESULT: Signal<CriticalSectionRawMutex, Result<(), TxError>> = Signal::new();

/// SX1302 counter and the `Instant` it was read at, while the concentrator runs.
static COUNTER: Mutex<Cell<Option<(u32, Instant)>>> = Mutex::new(Cell::new(None));

/// Frames and frames with a valid CRC since the previous `take_counters`.
static RX_RECEIVED: AtomicU32 = AtomicU32::new(0);
static RX_OK: AtomicU32 = AtomicU32::new(0);

pub fn status() -> Status {
    critical_section::with(|cs| STATUS.borrow(cs).get())
}

fn update(f: impl FnOnce(&mut Status)) {
    critical_section::with(|cs| {
        let c = STATUS.borrow(cs);
        let mut s = c.get();
        f(&mut s);
        c.set(s);
    });
}

/// The SX1302 1 MHz counter now, extrapolated from the last read (at most a poll
/// interval old; the two crystals drift apart by well under 1 µs in that time).
/// `None` while the concentrator is not running.
pub fn count_us() -> Option<u32> {
    let (cnt, at) = critical_section::with(|cs| COUNTER.borrow(cs).get())?;
    Some(cnt.wrapping_add(at.elapsed().as_micros() as u32))
}

fn set_counter(c: Option<(u32, Instant)>) {
    critical_section::with(|cs| COUNTER.borrow(cs).set(c));
}

/// Frames received and frames with a valid CRC since the previous call, for
/// `GatewayStats`.
pub fn take_counters() -> (u32, u32) {
    (RX_RECEIVED.swap(0, Ordering::Relaxed), RX_OK.swap(0, Ordering::Relaxed))
}

/// Asks core 1 to stop the HAL, power-cycle the module and start again.
pub fn restart() {
    RESTART.store(true, Ordering::Release);
}

/// Sends `tx` (timestamped on the SX1302 counter) and waits until the TX modem is free
/// again: after the frame is on air, or when it was not sent. One frame at a time.
pub async fn transmit(mut tx: Tx, airtime_us: u32) -> Result<(), TxError> {
    let Some(chain) = Conf::EU868.tx_chain() else {
        return Err(TxError::Failed("no TX radio"));
    };
    tx.rf_chain = chain;
    if status().state != State::Running {
        return Err(TxError::NotRunning);
    }
    TX_RESULT.reset();
    critical_section::with(|cs| *PENDING.borrow_ref_mut(cs) = Some(Pending { tx, airtime_us }));
    TX_RESULT.wait().await
}

fn take_pending() -> Option<Pending> {
    critical_section::with(|cs| PENDING.borrow_ref_mut(cs).take())
}

/// Answers a frame left while the concentrator was not running.
fn drop_pending() {
    if take_pending().is_some() {
        TX_RESULT.signal(Err(TxError::NotRunning));
    }
}

/// Hands the module to core 1, which brings it up and keeps receiving.
pub fn start(core1: Peri<'static, CORE1>, radio: Radio) {
    static STACK: ConstStaticCell<Stack<CORE1_STACK>> = ConstStaticCell::new(Stack::new());
    spawn_core1(core1, STACK.take(), move || run(radio));
}

fn run(radio: Radio) -> ! {
    let mut c = Concentrator::new(radio).unwrap();
    let mut pkts = [const { Packet::EMPTY }; sx1302::RX_MAX];
    loop {
        update(|s| {
            *s = Status {
                state: State::Starting,
                starts: s.starts + 1,
                probe: None,
                eui: None,
                rx_ok: 0,
                rx_bad: 0,
                tx_ok: 0,
                tx_failed: 0,
                tx_load_max_us: 0,
            }
        });
        set_counter(None);
        RESTART.store(false, Ordering::Relaxed);
        c.bus().power_cycle();
        let probe = c.bus().probe();
        update(|s| s.probe = Some(probe));
        if !probe.answered() {
            warn!("radio: no answer from the SX1302 ({probe})");
            update(|s| s.state = State::NoAnswer);
            wait_restart();
            continue;
        }
        if !probe.reset {
            // A fresh power-up still works, but a restart keeps the state of the last run.
            warn!("radio: reset did not clear the SX1302, check SX1302_RESET (GP14)");
        }
        info!("radio: SX1302 {probe}, starting the concentrator");
        let t = Instant::now();
        match c.start(&Conf::EU868) {
            Ok(eui) => {
                info!("radio: running after {} ms, EU868, eui {eui:016x}", t.elapsed().as_millis());
                update(|s| {
                    s.state = State::Running;
                    s.eui = Some(eui);
                });
            }
            Err(e) => {
                warn!("radio: {} failed", e.0);
                c.stop();
                update(|s| s.state = State::Failed(e.0));
                wait_restart();
                continue;
            }
        }
        drop_pending();
        let mut on_air = None;
        while !RESTART.load(Ordering::Acquire) {
            match c.receive(&mut pkts) {
                Ok(n) => {
                    let at = Instant::now();
                    for p in &pkts[..n] {
                        if received(p) {
                            // Dropped while offline.
                            let _ = RECEIVED.try_send(Received { packet: p.clone(), at });
                        }
                    }
                }
                Err(e) => warn!("radio: {} failed", e.0),
            }
            match c.counter_us() {
                Ok(cnt) => set_counter(Some((cnt, Instant::now()))),
                Err(e) => warn!("radio: {} failed", e.0),
            }
            // Until the next poll, load frames from core 0 and watch the one on air.
            let next = Instant::now() + POLL;
            while Instant::now() < next {
                if on_air.is_none()
                    && let Some(p) = take_pending()
                {
                    on_air = load(&mut c, p);
                }
                if let Some(t) = on_air
                    && let Some(r) = tx_finished(&mut c, t)
                {
                    finish(r);
                    on_air = None;
                }
                block_for(TX_STEP);
            }
        }
        info!("radio: restart");
        if let Some(OnAir { chain, .. }) = on_air {
            let _ = c.tx_abort(chain);
            finish(Err(TxError::NotRunning));
        }
        set_counter(None);
        c.stop();
        drop_pending();
    }
}

fn wait_restart() {
    while !RESTART.load(Ordering::Acquire) {
        drop_pending();
        block_for(Duration::from_millis(10));
    }
}

/// A frame loaded into the TX modem.
#[derive(Clone, Copy)]
struct OnAir {
    chain: u8,
    start: u32,
    end: u32,
}

/// The SX1302 counter, read now (`count_us` may be up to a poll old).
fn counter(c: &mut Concentrator<Radio>) -> u32 {
    match c.counter_us() {
        Ok(cnt) => {
            set_counter(Some((cnt, Instant::now())));
            cnt
        }
        Err(_) => count_us().unwrap_or(0),
    }
}

/// Loads `p` into the TX modem; `None` if it was not sent (answered already).
fn load(c: &mut Concentrator<Radio>, p: Pending) -> Option<OnAir> {
    let Pending { tx, airtime_us } = p;
    let chain = tx.rf_chain;
    let start = tx.count_us;
    match c.tx_status(chain) {
        Ok(TxStatus::Free) => {}
        Ok(st) => {
            finish(Err(TxError::Failed(match st {
                TxStatus::Off => "TX modem off",
                TxStatus::Scheduled | TxStatus::Emitting => "TX modem busy",
                _ => "TX modem state unknown",
            })));
            return None;
        }
        Err(e) => {
            finish(Err(TxError::Failed(e.0)));
            return None;
        }
    }
    // lgw_send has to finish before the HAL's trigger, TX_START_DELAY_US early.
    let lead = jit::offset(start, counter(c));
    if lead < (jit::LGW_SEND_US + jit::TX_START_DELAY_US) as i32 {
        finish(Err(TxError::Late { lead_us: lead }));
        return None;
    }
    let t = Instant::now();
    if let Err(e) = c.send(&tx) {
        finish(Err(TxError::Failed(e.0)));
        return None;
    }
    let took = t.elapsed().as_micros() as u32;
    update(|s| s.tx_load_max_us = s.tx_load_max_us.max(took));
    let lead = jit::offset(start, counter(c));
    info!("radio: tx loaded in {took} µs, {lead} µs before its start");
    Some(OnAir { chain, start, end: start.wrapping_add(airtime_us) })
}

/// The result once the frame is done: sent, or stuck and stopped.
fn tx_finished(c: &mut Concentrator<Radio>, t: OnAir) -> Option<Result<(), TxError>> {
    let now = count_us()?;
    let past_end = jit::offset(now, t.end);
    if past_end < 0 {
        return None;
    }
    let st = match c.tx_status(t.chain) {
        Ok(st) => st,
        Err(e) => return Some(Err(TxError::Failed(e.0))),
    };
    match st {
        TxStatus::Free => Some(Ok(())),
        // Loaded after its trigger time: the modem would wait for the counter to wrap.
        TxStatus::Scheduled => {
            let _ = c.tx_abort(t.chain);
            Some(Err(TxError::Late { lead_us: -jit::offset(now, t.start) }))
        }
        _ if past_end > TX_OVERRUN_US => {
            let _ = c.tx_abort(t.chain);
            Some(Err(TxError::Failed("still on air after its airtime")))
        }
        _ => None,
    }
}

fn finish(r: Result<(), TxError>) {
    update(|s| match r {
        Ok(()) => s.tx_ok += 1,
        Err(_) => s.tx_failed += 1,
    });
    TX_RESULT.signal(r);
}

/// Logs and counts a frame; true if it is to be forwarded (valid CRC, as lora_pkt_fwd
/// by default).
fn received(p: &Packet) -> bool {
    let crc = match p.crc() {
        Crc::Ok => "crc ok",
        Crc::Bad => "crc error",
        Crc::None => "no crc",
        Crc::Unknown => "crc unknown",
    };
    update(|s| match p.crc() {
        Crc::Ok => s.rx_ok += 1,
        Crc::Bad => s.rx_bad += 1,
        Crc::None | Crc::Unknown => {}
    });
    RX_RECEIVED.fetch_add(1, Ordering::Relaxed);
    let ok = matches!(p.crc(), Crc::Ok);
    if ok {
        RX_OK.fetch_add(1, Ordering::Relaxed);
    }
    let (mhz, khz) = (p.freq_hz / 1_000_000, p.freq_hz % 1_000_000 / 1000);
    let len = p.payload().len();
    match p.modulation() {
        Modulation::Lora { sf, bandwidth_hz, coderate } => info!(
            "radio: rx {mhz}.{khz:03} MHz SF{sf} BW{} 4/{coderate}, {crc}, rssi {:.0} dBm, snr {:.1} dB, {len} bytes, count_us {}",
            bandwidth_hz / 1000,
            p.rssi,
            p.snr,
            p.count_us
        ),
        Modulation::Fsk { datarate } => info!(
            "radio: rx {mhz}.{khz:03} MHz FSK {datarate} bit/s, {crc}, rssi {:.0} dBm, {len} bytes, count_us {}",
            p.rssi, p.count_us
        ),
    }
    ok
}

/// Publishes the frames from core 1 as `UplinkFrame`s (through `mqtt::UPLINKS`, which
/// drops them while disconnected).
#[embassy_executor::task]
pub async fn uplink_task(gateway_id: &'static str) -> ! {
    loop {
        let Received { packet: p, at } = RECEIVED.receive().await;
        let Modulation::Lora { sf, bandwidth_hz, coderate } = p.modulation() else {
            // ChirpStack's FSK data rate (DR7) is not used by the EU868 devices here.
            warn!("radio: FSK frame not forwarded");
            continue;
        };
        let code_rate = match coderate {
            5 => chirpstack::gw::CodeRate::Cr45,
            6 => chirpstack::gw::CodeRate::Cr46,
            7 => chirpstack::gw::CodeRate::Cr47,
            8 => chirpstack::gw::CodeRate::Cr48,
            _ => chirpstack::gw::CodeRate::CrUndefined,
        };
        let mut id = [0u8; 4];
        crate::tls::random_bytes(&mut id);
        let rx = chirpstack::LoraRx {
            uplink_id: u32::from_be_bytes(id),
            frequency: p.freq_hz,
            bandwidth: bandwidth_hz,
            spreading_factor: sf.into(),
            code_rate,
            rssi: round(p.rssi),
            snr: p.snr,
            channel: p.if_chain.into(),
            rf_chain: p.rf_chain.into(),
            count_us: p.count_us,
            crc_ok: true,
            time_us: clock::base_us().map(|b| b + at.as_micros() as i64),
        };
        let mut u = Uplink { len: 0, buf: [0; 512] };
        match chirpstack::uplink_frame(gateway_id, p.payload(), &rx, &mut u.buf) {
            Ok(len) => u.len = len,
            Err(_) => {
                warn!("radio: {} byte frame does not fit an UplinkFrame", p.payload().len());
                continue;
            }
        }
        if UPLINKS.try_send(u).is_err() {
            warn!("radio: uplink queue full, frame dropped");
        }
    }
}

/// To the nearest whole number (`f32::round` needs std).
fn round(x: f32) -> i32 {
    (if x < 0.0 { x - 0.5 } else { x + 0.5 }) as i32
}
