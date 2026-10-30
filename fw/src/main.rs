//! picocell-gw firmware: W5500-EVB-Pico2 → ChirpStack over MQTT/TLS (see sketch.md).
//!
//! Current state: Ethernet (DHCP or static), hosts table + DNS, SNTP, TLS 1.3 + MQTT 5
//! session with ChirpStack protobuf messages: retained ConnState with Last Will,
//! GatewayStats every 30 s, UplinkFrames from the WM1302 (sx1302_hal on core 1), and
//! DownlinkFrames scheduled in the JIT queue on the SX1302 counter and acknowledged;
//! downlinks are logged at their start, not yet sent.
//! Log output, console commands and provisioning on the USB-C port (CDC-ACM); the
//! config lives encrypted in flash, and without one the gateway stays offline.

#![no_std]
#![no_main]

mod boot;
mod ca_update;
mod clock;
mod config;
mod config_key;
mod console;
mod downlink;
mod flash;
mod ids;
mod login;
mod mqtt;
mod ota;
mod radio;
mod resolve;
mod sntp;
mod tls;

use embassy_executor::Spawner;
use embassy_net::{Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_net_wiznet::chip::W5500;
use embassy_net_wiznet::{Device, Runner, State};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, SPI0, TRNG, USB};
use embassy_rp::spi::{Async, Config as SpiConfig, Spi};
use embassy_rp::trng::Trng;
use embassy_rp::usb::Driver;
use embassy_rp::watchdog::Watchdog;
use embassy_rp::{bind_interrupts, dma, trng, usb};
use embassy_time::Delay;
use embedded_hal_bus::spi::ExclusiveDevice;
use log::info;
use panic_halt as _;
use static_cell::StaticCell;
use wire::time::Utc;

use crate::config::Net;
use crate::ids::Ids;
use crate::mqtt::Buffers;
use crate::radio::Radio;

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
    TRNG_IRQ => trng::InterruptHandler<TRNG>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

type W5500Spi = ExclusiveDevice<Spi<'static, SPI0, Async>, Output<'static>, Delay>;

#[embassy_executor::task]
async fn ethernet_task(runner: Runner<'static, W5500, W5500Spi, Input<'static>, Output<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device<'static>>) -> ! {
    runner.run().await
}

/// Stand-in for the radio while it is not running (no module on the desk): an
/// `UplinkFrame` every 10 s with made-up radio metadata, for tools/hil-test.py and
/// tools/test-downlink.sh. The payload is a LoRaWAN proprietary frame (MHDR 0xe0),
/// which no device claims. Not in production builds.
#[cfg(not(feature = "production"))]
#[embassy_executor::task]
async fn test_uplink_task(gateway_id: &'static str) -> ! {
    use crate::mqtt::{UPLINKS, Uplink};
    use embassy_time::Timer;

    let mut n = 0u8;
    loop {
        Timer::after_secs(10).await;
        if radio::status().state == radio::State::Running {
            continue;
        }
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
            count_us: downlink::count_us(),
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

/// Logs each change of the IPv4 configuration (DHCP lease, link loss).
#[embassy_executor::task]
async fn net_log_task(stack: Stack<'static>) -> ! {
    loop {
        stack.wait_config_up().await;
        if let Some(c) = stack.config_v4() {
            match c.gateway {
                Some(g) => info!("network up: {}, gateway {g}", c.address),
                None => info!("network up: {}, no gateway", c.address),
            }
        }
        stack.wait_config_down().await;
        info!("network down");
    }
}

#[embassy_executor::main(executor = "embassy_rp::executor::Executor", entry = "cortex_m_rt::entry")]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    console::init();

    tls::init_rng(Trng::new(p.TRNG, Irqs, Default::default()));

    static IDS: StaticCell<Ids> = StaticCell::new();
    let ids: &'static Ids = IDS.init(Ids::read());
    let m = ids.mac;
    info!(
        "picocell-gw {}, built {}, gateway id {}, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        env!("CARGO_PKG_VERSION"),
        Utc(config::build_time()),
        ids.gateway_id, m[0], m[1], m[2], m[3], m[4], m[5]
    );

    boot::start();
    spawner.spawn(boot::watchdog_task(Watchdog::new(p.WATCHDOG)).unwrap());
    config::init().await;

    let radio = Radio::new(
        p.SPI1, p.PIN_10, p.PIN_11, p.PIN_12, p.PIN_13, p.PIN_14, p.PIN_15, p.I2C0, p.PIN_4, p.PIN_5,
    );
    radio::start(p.CORE1, radio);

    let usb = Driver::new(p.USB, Irqs);
    if !config::provisioned() || boot::refuse_online().is_some() {
        // Offline until configured on the console (sketch.md section 5.4), and a
        // production build on a board that is not locked for good.
        spawner.spawn(console::console_task(usb, console::Context { stack: None, ids }).unwrap());
        return;
    }

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

    let net_config = match config::get().net() {
        Some(Net::Dhcp) => embassy_net::Config::dhcpv4(Default::default()),
        Some(Net::Static { address, prefix, gateway }) => embassy_net::Config::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(address, prefix),
            gateway,
            dns_servers: Default::default(),
        }),
        // Static without an address; `provisioned()` rules that out.
        None => embassy_net::Config::default(),
    };

    // DHCP, MQTT (TCP), SNTP (UDP) and one DNS lookup each from the MQTT and SNTP tasks.
    static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    // Seeds local ports, TCP sequence numbers and DHCP transaction IDs.
    let mut seed = [0u8; 8];
    tls::random_bytes(&mut seed);
    let (stack, runner) = embassy_net::new(
        device,
        net_config,
        RESOURCES.init(StackResources::new()),
        u64::from_le_bytes(seed),
    );
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(net_log_task(stack).unwrap());
    spawner.spawn(console::console_task(usb, console::Context { stack: Some(stack), ids }).unwrap());
    spawner.spawn(sntp::sntp_task(stack).unwrap());

    static BUFFERS: StaticCell<Buffers> = StaticCell::new();
    let bufs = BUFFERS.init(Buffers::new());
    spawner.spawn(mqtt::mqtt_task(stack, ids.gateway_id.as_str(), bufs).unwrap());
    spawner.spawn(radio::uplink_task(ids.gateway_id.as_str()).unwrap());
    #[cfg(not(feature = "production"))]
    spawner.spawn(test_uplink_task(ids.gateway_id.as_str()).unwrap());
    spawner.spawn(downlink::tx_task().unwrap());
}
