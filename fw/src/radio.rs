//! WM1302 bring-up (sketch.md section 8, step 2): SPI1 to the SX1302, power and reset,
//! chip version and EUI. No sx1302_hal yet, so the radio does not receive or send.
//!
//! Pins (sketch.md section 2.3): SPI1 GP10 SCK, GP11 MOSI, GP12 MISO, GP13 CS,
//! GP14 SX1302_RESET (active high), GP15 POWER_EN. While the module is off, all of
//! them stay low so nothing back-powers it through its I/O.
//!
//! SPI framing as in sx1302_hal `loragw_spi.c`: a mux byte (0x00 = SX1302), the
//! 15-bit address with bit 7 of the first byte set for a write, then the data; a read
//! clocks one dummy byte before the data.

use core::fmt;

use embassy_rp::Peri;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIN_10, PIN_11, PIN_12, PIN_13, PIN_14, PIN_15, SPI1};
use embassy_rp::spi::{Blocking, Config as SpiConfig, Spi};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::Timer;

/// sx1302_hal `loragw_reg.c`: COMMON_PAGE_PAGE (bits 1:0, read/write).
const REG_PAGE: u16 = 0x5600;
/// COMMON_VERSION_VERSION (read-only).
const REG_VERSION: u16 = 0x5606;
/// OTP_BYTE_ADDR_ADDR and OTP_RD_DATA_RD_DATA; the EUI is OTP bytes 0..8, MSB first.
const REG_OTP_ADDR: u16 = 0x6180;
const REG_OTP_DATA: u16 = 0x6181;
const MUX_SX1302: u8 = 0x00;
/// The HAL's Linux default (`SPI_SPEED`); fine over jumper wires.
const SPI_HZ: u32 = 2_000_000;

pub type Shared = Mutex<CriticalSectionRawMutex, Radio>;

pub struct Radio {
    spi: Spi<'static, SPI1, Blocking>,
    cs: Output<'static>,
    reset: Output<'static>,
    power: Output<'static>,
}

/// What `probe` found.
pub struct Probe {
    pub version: u8,
    /// The page register read back what was written (MOSI, MISO, SCK and CS work).
    pub loopback: bool,
    pub eui: u64,
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
        write!(f, "page register {}, ", if self.loopback { "ok" } else { "failed" })?;
        write!(f, "eui {:016x}", self.eui)
    }
}

impl Radio {
    /// Takes the pins with the module off.
    pub fn new(
        spi: Peri<'static, SPI1>,
        sck: Peri<'static, PIN_10>,
        mosi: Peri<'static, PIN_11>,
        miso: Peri<'static, PIN_12>,
        cs: Peri<'static, PIN_13>,
        reset: Peri<'static, PIN_14>,
        power: Peri<'static, PIN_15>,
    ) -> Self {
        let mut cfg = SpiConfig::default();
        cfg.frequency = SPI_HZ;
        Self {
            spi: Spi::new_blocking(spi, sck, mosi, miso, cfg),
            cs: Output::new(cs, Level::Low),
            reset: Output::new(reset, Level::Low),
            power: Output::new(power, Level::Low),
        }
    }

    /// Switches the module off (CS low first) and on again, then resets the SX1302.
    pub async fn power_cycle(&mut self) {
        self.cs.set_low();
        self.reset.set_low();
        self.power.set_low();
        Timer::after_millis(100).await;
        self.power.set_high();
        // Buck (or the Pi HAT regulator) up and settled.
        Timer::after_millis(100).await;
        self.cs.set_high();
        self.reset.set_high();
        Timer::after_millis(10).await;
        self.reset.set_low();
        Timer::after_millis(10).await;
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

    /// Reads the version, writes and reads back the page register, reads the EUI.
    pub fn probe(&mut self) -> Probe {
        let version = self.read(REG_VERSION);
        let loopback = [1, 2, 3, 0].into_iter().all(|p| {
            self.write(REG_PAGE, p);
            self.read(REG_PAGE) & 0x03 == p
        });
        self.write(REG_PAGE, 0);
        let mut eui = 0u64;
        for i in 0..8 {
            self.write(REG_OTP_ADDR, i);
            eui = eui << 8 | u64::from(self.read(REG_OTP_DATA));
        }
        Probe { version, loopback, eui }
    }
}

/// Power-cycles the module and probes the SX1302.
pub async fn bring_up(radio: &Shared) -> Probe {
    let mut r = radio.lock().await;
    r.power_cycle().await;
    r.probe()
}
