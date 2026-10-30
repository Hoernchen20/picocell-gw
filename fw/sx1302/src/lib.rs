//! Semtech sx1302_hal V2.1.0 (`hal/`, unmodified) for the SX1302 on the WM1302
//! (sketch.md section 3.1). `shim/` replaces the HAL's Linux SPI, I2C, clock and stdio
//! code with the [`Bus`] callbacks here; HAL output goes to the `log` crate.
//!
//! The HAL blocks (firmware upload, calibration, SPI polling) and keeps global state:
//! there is one [`Concentrator`], and all calls must come from the same core.

#![no_std]

use core::ffi::{c_char, c_int};
use core::mem::{offset_of, size_of};
use core::ptr::NonNull;
use core::slice;
use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy)]
pub struct BusError;

pub enum Data<'a> {
    Write(&'a [u8]),
    /// Clocked out and replaced by what was read.
    Exchange(&'a mut [u8]),
}

/// What the HAL needs from the board.
pub trait Bus {
    /// One SPI transaction with CS low: `cmd`, then `data`.
    fn spi(&mut self, cmd: &[u8], data: Data<'_>) -> Result<(), BusError>;
    /// Writes `wr`, then reads `rd` after a repeated start (write only if `rd` is empty).
    fn i2c(&mut self, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), BusError>;
    fn delay_us(&mut self, us: u32);
    /// Monotonic time in microseconds.
    fn now_us(&mut self) -> u64;
}

/// One radio (SX1250) of the concentrator (shim `struct pc_radio`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Radio {
    pub freq_hz: u32,
    pub rssi_offset: f32,
    /// RSSI temperature compensation, coeff_a..coeff_e.
    pub rssi_tcomp: [f32; 5],
    pub enable: bool,
    pub tx_enable: bool,
    _pad: [u8; 2],
}

/// An IF channel, relative to its radio (shim `struct pc_chan`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Chan {
    pub if_hz: i32,
    pub enable: bool,
    pub radio: u8,
    _pad: [u8; 2],
}

/// Channel plan and board settings (shim `struct pc_conf`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Conf {
    pub radio: [Radio; 2],
    /// Multi-SF LoRa channels, 125 kHz.
    pub multi_sf: [Chan; 8],
    pub lora_std: Chan,
    pub fsk: Chan,
    pub lora_std_bandwidth_hz: u32,
    pub fsk_bandwidth_hz: u32,
    pub fsk_datarate: u32,
    pub lora_std_sf: u8,
    /// Spreading factors of the multi-SF channels, bit n = SF5 + n.
    pub multi_sf_mask: u8,
    pub lorawan_public: bool,
    /// Radio that clocks the SX1302.
    pub clksrc: u8,
}

const _: () = assert!(size_of::<Conf>() == 160 && offset_of!(Conf, clksrc) == 159);

const fn radio(freq_hz: u32, tx_enable: bool) -> Radio {
    Radio {
        freq_hz,
        // WM1302 SPI EU868 (global_conf.json.sx1250.EU868 in sx1302_hal V2.1.0).
        rssi_offset: -215.4,
        rssi_tcomp: [0.0, 0.0, 20.41, 2162.56, 0.0],
        enable: true,
        tx_enable,
        _pad: [0; 2],
    }
}

const fn chan(radio: u8, if_hz: i32) -> Chan {
    Chan { if_hz, enable: true, radio, _pad: [0; 2] }
}

impl Conf {
    /// The EU868 plan of `global_conf.json.sx1250.EU868` (sx1302_hal V2.1.0): radio 0 at
    /// 867.5 MHz (TX), radio 1 at 868.5 MHz; 867.1–868.5 MHz multi-SF, 868.3 MHz SF7
    /// 250 kHz, FSK 50 kbit/s at 868.8 MHz.
    pub const EU868: Conf = Conf {
        radio: [radio(867_500_000, true), radio(868_500_000, false)],
        multi_sf: [
            chan(1, -400_000),
            chan(1, -200_000),
            chan(1, 0),
            chan(0, -400_000),
            chan(0, -200_000),
            chan(0, 0),
            chan(0, 200_000),
            chan(0, 400_000),
        ],
        lora_std: chan(1, -200_000),
        fsk: chan(1, 300_000),
        lora_std_bandwidth_hz: 250_000,
        fsk_bandwidth_hz: 125_000,
        fsk_datarate: 50_000,
        lora_std_sf: 7,
        multi_sf_mask: 0xff,
        lorawan_public: true,
        clksrc: 0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modulation {
    Lora { sf: u8, bandwidth_hz: u32, coderate: u8 },
    Fsk { datarate: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crc {
    None,
    Ok,
    Bad,
    Unknown,
}

/// A received frame (shim `struct pc_rx`).
#[repr(C)]
#[derive(Clone)]
pub struct Packet {
    pub freq_hz: u32,
    /// SX1302 counter at the end of the frame, 1 MHz.
    pub count_us: u32,
    bandwidth_hz: u32,
    datarate: u32,
    /// Channel RSSI, dBm.
    pub rssi: f32,
    /// LoRa only, dB.
    pub snr: f32,
    size: u16,
    pub if_chain: u8,
    pub rf_chain: u8,
    modulation: u8,
    crc: u8,
    coderate: u8,
    _pad: u8,
    payload: [u8; 256],
}

const _: () = assert!(size_of::<Packet>() == 288 && offset_of!(Packet, payload) == 32);

impl Packet {
    pub const EMPTY: Packet = Packet {
        freq_hz: 0,
        count_us: 0,
        bandwidth_hz: 0,
        datarate: 0,
        rssi: 0.0,
        snr: 0.0,
        size: 0,
        if_chain: 0,
        rf_chain: 0,
        modulation: 0,
        crc: 0,
        coderate: 0,
        _pad: 0,
        payload: [0; 256],
    };

    pub fn payload(&self) -> &[u8] {
        &self.payload[..usize::from(self.size).min(self.payload.len())]
    }

    pub fn modulation(&self) -> Modulation {
        match self.modulation {
            1 => Modulation::Fsk { datarate: self.datarate },
            _ => Modulation::Lora { sf: self.datarate as u8, bandwidth_hz: self.bandwidth_hz, coderate: self.coderate },
        }
    }

    pub fn crc(&self) -> Crc {
        match self.crc {
            0 => Crc::None,
            1 => Crc::Ok,
            2 => Crc::Bad,
            _ => Crc::Unknown,
        }
    }
}

/// The HAL call that failed; the HAL logs the details.
#[derive(Debug, Clone, Copy)]
pub struct Error(pub &'static str);

unsafe extern "C" {
    fn pc_start(conf: *const Conf, eui: *mut u64) -> c_int;
    fn pc_stop() -> c_int;
    fn pc_receive(out: *mut Packet, max: u8) -> c_int;
    fn lgw_get_instcnt(inst_cnt_us: *mut u32) -> c_int;
}

/// Frames per `lgw_receive` call (shim `PC_RX_MAX`).
pub const RX_MAX: usize = 8;

static TAKEN: AtomicBool = AtomicBool::new(false);

/// The board while a HAL call runs; only read by value, from the calling core.
static mut BUS: Option<NonNull<dyn Bus>> = None;

pub struct Concentrator<B: Bus + 'static> {
    bus: B,
}

impl<B: Bus + 'static> Concentrator<B> {
    /// `None` after the first call: the HAL has one global context.
    pub fn new(bus: B) -> Option<Self> {
        match TAKEN.swap(true, Ordering::AcqRel) {
            true => None,
            false => Some(Self { bus }),
        }
    }

    /// The board, for what happens outside the HAL (power, reset).
    pub fn bus(&mut self) -> &mut B {
        &mut self.bus
    }

    fn call<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let bus: &mut (dyn Bus + 'static) = &mut self.bus;
        unsafe { BUS = Some(NonNull::from(bus)) };
        let r = f();
        unsafe { BUS = None };
        r
    }

    /// Configures and starts the concentrator (`lgw_start`: firmware upload, radio
    /// calibration, takes about a second) and returns the SX1302 EUI.
    pub fn start(&mut self, conf: &Conf) -> Result<u64, Error> {
        let mut eui = 0;
        match self.call(|| unsafe { pc_start(conf, &mut eui) }) {
            0 => Ok(eui),
            -1 => Err(Error("lgw_board_setconf")),
            -2 => Err(Error("lgw_rxrf_setconf")),
            -3 => Err(Error("lgw_demod_setconf")),
            -4 => Err(Error("lgw_rxif_setconf")),
            -5 => Err(Error("lgw_start")),
            _ => Err(Error("lgw_get_eui")),
        }
    }

    pub fn stop(&mut self) {
        self.call(|| unsafe { pc_stop() });
    }

    /// Fetches up to `pkts.len()` (at most [`RX_MAX`]) frames.
    pub fn receive(&mut self, pkts: &mut [Packet]) -> Result<usize, Error> {
        let max = pkts.len().min(RX_MAX) as u8;
        match self.call(|| unsafe { pc_receive(pkts.as_mut_ptr(), max) }) {
            n if n >= 0 => Ok(n as usize),
            _ => Err(Error("lgw_receive")),
        }
    }

    /// The SX1302 1 MHz counter now, the one `Packet::count_us` refers to.
    pub fn counter_us(&mut self) -> Result<u32, Error> {
        let mut cnt = 0;
        match self.call(|| unsafe { lgw_get_instcnt(&mut cnt) }) {
            0 => Ok(cnt),
            _ => Err(Error("lgw_get_instcnt")),
        }
    }
}

fn with_bus<R>(f: impl FnOnce(&mut dyn Bus) -> R) -> Option<R> {
    // Set by `Concentrator::call` around the HAL call that ends up here.
    unsafe { BUS }.map(|mut b| f(unsafe { b.as_mut() }))
}

/// A slice from C; a null or dangling pointer is fine for length 0.
unsafe fn bytes<'a>(p: *const u8, len: u16) -> &'a [u8] {
    match len {
        0 => &[],
        n => unsafe { slice::from_raw_parts(p, n.into()) },
    }
}

unsafe fn bytes_mut<'a>(p: *mut u8, len: u16) -> &'a mut [u8] {
    match len {
        0 => &mut [],
        n => unsafe { slice::from_raw_parts_mut(p, n.into()) },
    }
}

#[unsafe(no_mangle)]
extern "C" fn pc_spi(cmd: *const u8, cmd_len: u16, data: *mut u8, len: u16, exchange: bool) -> c_int {
    let cmd = unsafe { bytes(cmd, cmd_len) };
    let data = match exchange {
        true => Data::Exchange(unsafe { bytes_mut(data, len) }),
        false => Data::Write(unsafe { bytes(data, len) }),
    };
    match with_bus(|b| b.spi(cmd, data)) {
        Some(Ok(())) => 0,
        _ => -1,
    }
}

#[unsafe(no_mangle)]
extern "C" fn pc_i2c(addr: u8, wr: *const u8, wr_len: u16, rd: *mut u8, rd_len: u16) -> c_int {
    let (wr, rd) = unsafe { (bytes(wr, wr_len), bytes_mut(rd, rd_len)) };
    match with_bus(|b| b.i2c(addr, wr, rd)) {
        Some(Ok(())) => 0,
        _ => -1,
    }
}

#[unsafe(no_mangle)]
extern "C" fn pc_delay_us(us: u32) {
    with_bus(|b| b.delay_us(us));
}

#[unsafe(no_mangle)]
extern "C" fn pc_now_us() -> u64 {
    with_bus(|b| b.now_us()).unwrap_or(0)
}

#[unsafe(no_mangle)]
extern "C" fn pc_log(level: c_int, msg: *const c_char, len: usize) {
    let msg = unsafe { slice::from_raw_parts(msg.cast::<u8>(), len) };
    let msg = core::str::from_utf8(msg).unwrap_or("(not UTF-8)");
    match level {
        1 => log::error!("sx1302_hal: {msg}"),
        2 => log::warn!("sx1302_hal: {msg}"),
        _ => log::info!("sx1302_hal: {msg}"),
    }
}

#[unsafe(no_mangle)]
extern "C" fn pc_abort() -> ! {
    panic!("sx1302_hal: abort");
}
