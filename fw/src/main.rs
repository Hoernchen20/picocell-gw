//! picocell-gw firmware: W5500-EVB-Pico2 → ChirpStack over MQTT/TLS (see sketch.md).
//!
//! Current state: Ethernet (DHCP or static), TLS 1.3 + MQTT 5 session with retained
//! ConnState, Last Will, command/down subscription and a test uplink every 10 s. No
//! radio yet. Log output on the USB-C port (CDC-ACM).

#![no_std]
#![no_main]

mod config;
mod ids;
mod mqtt;
mod tls;

use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_net_wiznet::chip::W5500;
use embassy_net_wiznet::{Device, Runner, State};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, SPI0, TRNG, USB};
use embassy_rp::spi::{Async, Config as SpiConfig, Spi};
use embassy_rp::trng::Trng;
use embassy_rp::usb::Driver;
use embassy_rp::{bind_interrupts, dma, trng, usb};
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use log::{info, warn};
use panic_halt as _;
use static_cell::StaticCell;

use crate::config::Net;
use crate::ids::Ids;
use crate::mqtt::{Buffers, UPLINKS, Uplink};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
    TRNG_IRQ => trng::InterruptHandler<TRNG>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

type W5500Spi = ExclusiveDevice<Spi<'static, SPI0, Async>, Output<'static>, Delay>;

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn ethernet_task(runner: Runner<'static, W5500, W5500Spi, Input<'static>, Output<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device<'static>>) -> ! {
    runner.run().await
}

/// Stand-in for the radio: a small test payload every 10 s.
#[embassy_executor::task]
async fn test_uplink_task() -> ! {
    let mut n = 0u8;
    loop {
        Timer::after_secs(10).await;
        n = n.wrapping_add(1);
        let mut u = Uplink { len: 16, buf: [0; 512] };
        u.buf[..12].copy_from_slice(b"test uplink ");
        u.buf[12] = n;
        let _ = UPLINKS.try_send(u);
    }
}

/// Repeats the board identity and network state, since the USB console usually
/// connects after boot.
#[embassy_executor::task]
async fn status_task(stack: Stack<'static>, ids: &'static Ids) -> ! {
    loop {
        let m = ids.mac;
        info!(
            "gateway id {} | mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} | ip {:?} | build {}",
            ids.gateway_id, m[0], m[1], m[2], m[3], m[4], m[5],
            stack.config_v4().map(|c| c.address),
            config::build_time()
        );
        Timer::after_secs(30).await;
    }
}

#[embassy_executor::main(executor = "embassy_rp::executor::Executor", entry = "cortex_m_rt::entry")]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    spawner.spawn(logger_task(Driver::new(p.USB, Irqs)).unwrap());

    tls::init_rng(Trng::new(p.TRNG, Irqs, Default::default()));
    // TODO: SNTP. Until then the build time stands in, so certificate dates are
    // checked against it and the fail-closed path is never taken.
    tls::set_time(config::build_time());

    static IDS: StaticCell<Ids> = StaticCell::new();
    let ids: &'static Ids = IDS.init(Ids::read());

    // W5500 on the EVB: SPI0 GP16 MISO, GP17 CS, GP18 SCK, GP19 MOSI, GP20 RST, GP21 INT.
    let mut spi_cfg = SpiConfig::default();
    spi_cfg.frequency = 50_000_000;
    let spi = Spi::new(p.SPI0, p.PIN_18, p.PIN_19, p.PIN_16, p.DMA_CH0, p.DMA_CH1, Irqs, spi_cfg);
    let cs = Output::new(p.PIN_17, Level::High);
    let int = Input::new(p.PIN_21, Pull::Up);
    let reset = Output::new(p.PIN_20, Level::High);

    static STATE: StaticCell<State<8, 8>> = StaticCell::new();
    let (device, runner) = embassy_net_wiznet::new(
        ids.mac,
        STATE.init(State::new()),
        ExclusiveDevice::new(spi, cs, Delay),
        int,
        reset,
    )
    .await
    .unwrap();
    spawner.spawn(ethernet_task(runner).unwrap());

    let net_config = match config::net() {
        Some(Net::Dhcp) => embassy_net::Config::dhcpv4(Default::default()),
        Some(Net::Static(c)) => embassy_net::Config::ipv4_static(c),
        None => {
            // Stay offline (sketch.md section 3.4); the status task keeps reporting.
            warn!("invalid PICOCELL_NET, network disabled");
            embassy_net::Config::default()
        }
    };
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device,
        net_config,
        RESOURCES.init(StackResources::new()),
        RoscRng.next_u64(),
    );
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(status_task(stack, ids).unwrap());

    static BUFFERS: StaticCell<Buffers> = StaticCell::new();
    let bufs = BUFFERS.init(Buffers::new());
    spawner.spawn(mqtt::mqtt_task(stack, ids.gateway_id.as_str(), bufs).unwrap());
    spawner.spawn(test_uplink_task().unwrap());
}
