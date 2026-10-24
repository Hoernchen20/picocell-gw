//! picocell-gw firmware: W5500-EVB-Pico2 → ChirpStack over MQTT/TLS (see sketch.md).
//!
//! Current state: Ethernet (DHCP or static), hosts table + DNS, SNTP, TLS 1.3 + MQTT 5
//! session with ChirpStack protobuf messages: retained ConnState with Last Will,
//! GatewayStats every 30 s, command/down subscription and a test UplinkFrame every 10 s.
//! No radio yet. Log output on the USB-C port (CDC-ACM).

#![no_std]
#![no_main]

mod clock;
mod config;
mod ids;
mod mqtt;
mod resolve;
mod sntp;
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
use embassy_time::{Delay, Instant, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use log::{info, warn};
use panic_halt as _;
use static_cell::StaticCell;
use wire::time::Utc;

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

/// Stand-in for the radio: an `UplinkFrame` every 10 s with made-up radio metadata.
/// The payload is a LoRaWAN proprietary frame (MHDR 0xe0), which no device claims.
#[embassy_executor::task]
async fn test_uplink_task(gateway_id: &'static str) -> ! {
    let mut n = 0u8;
    loop {
        Timer::after_secs(10).await;
        n = n.wrapping_add(1);
        let mut id = [0u8; 4];
        tls::random_bytes(&mut id);
        let rx = chirpstack::LoraRx {
            uplink_id: u32::from_be_bytes(id),
            frequency: 868_100_000,
            bandwidth: 125_000,
            spreading_factor: 7,
            code_rate: chirpstack::gw::CodeRate::Cr45,
            rssi: -60,
            snr: 9.5,
            channel: 0,
            rf_chain: 0,
            count_us: Instant::now().as_micros() as u32,
            crc_ok: true,
            time_us: clock::now_unix_us(),
        };
        let mut payload = *b"\xe0picocell test \0";
        payload[payload.len() - 1] = n;
        let mut u = Uplink { len: 0, buf: [0; 512] };
        match chirpstack::uplink_frame(gateway_id, &payload, &rx, &mut u.buf) {
            Ok(len) => u.len = len,
            Err(_) => continue,
        }
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
            "gateway id {} | mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} | ip {:?} | {} | build {}",
            ids.gateway_id, m[0], m[1], m[2], m[3], m[4], m[5],
            stack.config_v4().map(|c| c.address),
            clock::Now,
            Utc(config::build_time())
        );
        Timer::after_secs(30).await;
    }
}

#[embassy_executor::main(executor = "embassy_rp::executor::Executor", entry = "cortex_m_rt::entry")]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    spawner.spawn(logger_task(Driver::new(p.USB, Irqs)).unwrap());

    tls::init_rng(Trng::new(p.TRNG, Irqs, Default::default()));

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
    if config::hosts().is_none() {
        warn!("invalid PICOCELL_HOSTS, hosts table ignored");
    }
    if config::dns_servers().is_none() {
        warn!("invalid PICOCELL_DNS, DNS disabled");
    }

    // DHCP, MQTT (TCP), SNTP (UDP) and one DNS lookup each from the MQTT and SNTP tasks.
    static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device,
        net_config,
        RESOURCES.init(StackResources::new()),
        RoscRng.next_u64(),
    );
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(status_task(stack, ids).unwrap());
    spawner.spawn(sntp::sntp_task(stack).unwrap());

    static BUFFERS: StaticCell<Buffers> = StaticCell::new();
    let bufs = BUFFERS.init(Buffers::new());
    spawner.spawn(mqtt::mqtt_task(stack, ids.gateway_id.as_str(), bufs).unwrap());
    spawner.spawn(test_uplink_task(ids.gateway_id.as_str()).unwrap());
}
