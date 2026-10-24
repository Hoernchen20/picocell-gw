//! MQTT task: TCP → TLS → MQTT 5 session with ChirpStack's topics (sketch.md section 3.3).
//!
//! One task owns the connection and selects between incoming packets (downlinks, each
//! answered with a `DownlinkTxAck`), queued uplinks, stats and keepalive pings. Only `poll_header()` is cancel-safe, so it is the one that
//! sits in the select; `poll_body()` always runs to completion.

use core::fmt::Write;
use core::num::NonZero;
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_futures::select::{Either4, select4};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Ticker, Timer};
use embedded_tls::{TlsConfig, TlsConnection, TlsContext};
use log::{info, warn};
use jit::duty::{self, EU868};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::event::Event;
use rust_mqtt::client::options::{
    ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference, WillOptions,
};
use rust_mqtt::config::KeepAlive;
use rust_mqtt::types::{MqttBinary, MqttString, TopicFilter, TopicName};

use wire::time::Utc;

use crate::tls::{FailClosed, Suite};
use crate::{clock, config, downlink, resolve};

const KEEP_ALIVE_S: u16 = 30;
const MAX_PACKET: u32 = 8192;
/// ChirpStack's default `stats_interval`.
const STATS_INTERVAL_S: u64 = 30;

/// An encoded `UplinkFrame` (or other event) waiting to be published.
pub struct Uplink {
    pub len: usize,
    pub buf: [u8; 512],
}

pub static UPLINKS: Channel<CriticalSectionRawMutex, Uplink, 8> = Channel::new();

/// A full TLS record (16 KiB + 256 B overhead), so the broker's record size does not
/// matter (no reliance on the max_fragment_length extension).
const TLS_RX: usize = 16640;

pub struct Buffers {
    tcp_rx: [u8; 4352],
    tcp_tx: [u8; 4352],
    tls_rx: [u8; TLS_RX],
    tls_tx: [u8; 4352],
    mqtt: [u8; MAX_PACKET as usize],
}

impl Buffers {
    pub const fn new() -> Self {
        Self {
            tcp_rx: [0; 4352],
            tcp_tx: [0; 4352],
            tls_rx: [0; TLS_RX],
            tls_tx: [0; 4352],
            mqtt: [0; MAX_PACKET as usize],
        }
    }
}

/// Connection state for the console's `status`.
#[derive(Clone, Copy)]
enum State {
    Disabled,
    WaitNetwork,
    WaitTime,
    Connecting,
    Connected,
    Backoff,
}

/// Disabled until the task starts, which it does not in provisioning mode.
static STATE: AtomicU8 = AtomicU8::new(State::Disabled as u8);

fn set_state(s: State) {
    STATE.store(s as u8, Ordering::Relaxed);
}

/// The connection state in words.
pub fn state() -> &'static str {
    match STATE.load(Ordering::Relaxed) {
        x if x == State::Disabled as u8 => "disabled (provisioning mode)",
        x if x == State::WaitNetwork as u8 => "waiting for the network",
        x if x == State::WaitTime as u8 => "waiting for the time",
        x if x == State::Connecting as u8 => "connecting",
        x if x == State::Connected as u8 => "connected",
        _ => "disconnected, retrying",
    }
}

type Topic = heapless::String<80>;

fn topic(gateway_id: &str, suffix: &str) -> Topic {
    let mut t = Topic::new();
    let _ = write!(t, "{}/gateway/{}/{}", config::get().topic_prefix(), gateway_id, suffix);
    t
}

fn name(t: &Topic) -> TopicName<'_> {
    TopicName::new(MqttString::from_str(t.as_str()).unwrap()).unwrap()
}

#[embassy_executor::task]
pub async fn mqtt_task(
    stack: Stack<'static>,
    gateway_id: &'static str,
    bufs: &'static mut Buffers,
) -> ! {
    let port = config::get().mqtt_port();
    let mut backoff = 1u64;
    // CA slot to verify the broker with; after a failed handshake the other one, if set
    // (CA rotation, sketch.md section 5.3).
    let mut ca_slot = 0;
    loop {
        set_state(State::WaitNetwork);
        stack.wait_config_up().await;
        // Fail closed (sketch.md section 6): certificate dates cannot be checked yet.
        if !clock::is_set() {
            set_state(State::WaitTime);
            info!("mqtt: waiting for the time");
            while !clock::is_set() {
                Timer::after_secs(1).await;
            }
        }
        set_state(State::Connecting);
        match session(stack, port, ca_slot, gateway_id, bufs).await {
            // Was connected: retry soon.
            Ok(()) => backoff = 1,
            Err(e) => {
                warn!("mqtt: {e}");
                if e == TLS_FAILED && !config::get().ca(1 - ca_slot).is_empty() {
                    ca_slot = 1 - ca_slot;
                    info!("mqtt: trying tls.ca{ca_slot}");
                }
            }
        }
        set_state(State::Backoff);
        info!("mqtt: reconnecting in {backoff} s");
        Timer::after_secs(backoff).await;
        backoff = (backoff * 2).min(60);
    }
}

const TLS_FAILED: &str = "tls handshake failed";

/// One connection. `Ok` once the session was up and then ended, `Err` if it never came up.
async fn session(
    stack: Stack<'static>,
    port: u16,
    ca_slot: usize,
    gateway_id: &str,
    bufs: &mut Buffers,
) -> Result<(), &'static str> {
    let cfg = config::get();
    let host = cfg.mqtt_host();
    let addr = match resolve::resolve(stack, host).await {
        Ok(a) => a,
        Err(e) => {
            warn!("mqtt: {host}: {e}");
            return Err("broker address unknown");
        }
    };

    let state_topic = topic(gateway_id, "state/conn");
    let up_topic = topic(gateway_id, "event/up");
    let stats_topic = topic(gateway_id, "event/stats");
    let ack_topic = topic(gateway_id, "event/ack");
    let down_topic = topic(gateway_id, "command/down");

    let mut socket = TcpSocket::new(stack, &mut bufs.tcp_rx, &mut bufs.tcp_tx);
    // Also bounds poll_body(): never wrap MQTT reads in a timeout (not cancel-safe).
    socket.set_timeout(Some(Duration::from_secs(u64::from(KEEP_ALIVE_S) * 3)));
    socket.connect((addr, port)).await.map_err(|_| "tcp connect failed")?;

    let tls_config = TlsConfig::new().with_server_name(host);
    let mut tls: TlsConnection<'_, _, Suite> =
        TlsConnection::new(socket, &mut bufs.tls_rx, &mut bufs.tls_tx);
    tls.open(TlsContext::new(&tls_config, FailClosed::new(cfg.ca(ca_slot))))
        .await
        .map_err(|_| TLS_FAILED)?;

    let mut bump = BumpBuffer::new(&mut bufs.mqtt);
    let mut client: Client<'_, '_, _, _, 2, 4, 4, 0, 0, 0, 0> = Client::new(&mut bump);
    let mut offline = [0u8; 64];
    let n = chirpstack::conn_state(gateway_id, false, &mut offline).map_err(|_| "gateway id too long")?;
    let will = WillOptions::new(name(&state_topic), MqttBinary::try_from(&offline[..n]).unwrap())
        .at_least_once()
        .retain();
    let options = ConnectOptions::new()
        .clean_start()
        .keep_alive(KeepAlive::Seconds(NonZero::new(KEEP_ALIVE_S).unwrap()))
        .maximum_packet_size(NonZero::new(MAX_PACKET).unwrap())
        .user_name(MqttString::from_str(gateway_id).unwrap())
        .password(MqttBinary::try_from(cfg.mqtt_password().as_bytes()).unwrap())
        .will(will);
    client
        .connect(tls, &options, Some(MqttString::from_str(gateway_id).unwrap()))
        .await
        .map_err(|_| "mqtt connect refused")?;
    // Safety: the CONNACK info borrowing the buffer was dropped.
    unsafe { client.buffer_mut().reset() };
    info!("mqtt: connected to {host}");
    set_state(State::Connected);
    // Uplinks are not buffered across outages (sketch.md section 6): their RX windows
    // have passed, so drop whatever queued up while disconnected.
    let stale = UPLINKS.len();
    UPLINKS.clear();
    if stale > 0 {
        info!("mqtt: dropped {stale} queued uplinks");
    }

    let up = name(&up_topic);
    let mut online = [0u8; 64];
    let n = chirpstack::conn_state(gateway_id, true, &mut online).map_err(|_| "gateway id too long")?;
    let ok = client
        .publish(
            &PublicationOptions::new(TopicReference::Name(name(&state_topic)))
                .at_least_once()
                .retain(),
            online[..n].into(),
        )
        .await
        .is_ok()
        && client
            .subscribe(
                TopicFilter::new(MqttString::from_str(down_topic.as_str()).unwrap()).unwrap(),
                &SubscriptionOptions::new().at_least_once(),
            )
            .await
            .is_ok();
    if !ok {
        return Err("publish/subscribe failed");
    }

    let stats_options = PublicationOptions::new(TopicReference::Name(name(&stats_topic)));
    let ack_options = PublicationOptions::new(TopicReference::Name(name(&ack_topic)));
    let mut buf = [0u8; 512];
    // Right away, so ChirpStack shows the gateway online without waiting an interval.
    let n = stats(gateway_id, &mut buf);
    if let Err(e) = client.publish(&stats_options, buf[..n].into()).await {
        warn!("mqtt: stats: {e:?}");
        return Ok(());
    }

    let mut ping = Ticker::every(Duration::from_secs(u64::from(KEEP_ALIVE_S) * 2 / 3));
    let mut stats_ticker = Ticker::every(Duration::from_secs(STATS_INTERVAL_S));
    loop {
        match select4(client.poll_header(), UPLINKS.receive(), ping.next(), stats_ticker.next()).await {
            Either4::First(Ok(header)) => {
                // Only command/down is subscribed.
                let ack = match client.poll_body(header).await {
                    Ok(Event::Publish(p)) => downlink::handle(gateway_id, p.message.as_ref(), &mut buf),
                    Ok(_) => None,
                    Err(e) => {
                        warn!("mqtt: {e:?}");
                        break;
                    }
                };
                // Safety: the event borrowing the buffer was dropped.
                unsafe { client.buffer_mut().reset() };
                if let Some(n) = ack
                    && let Err(e) = client.publish(&ack_options, buf[..n].into()).await
                {
                    warn!("mqtt: ack: {e:?}");
                    break;
                }
            }
            Either4::First(Err(e)) => {
                warn!("mqtt: {e:?}");
                break;
            }
            Either4::Second(u) => {
                let options = PublicationOptions::new(TopicReference::Name(up.as_borrowed()));
                if let Err(e) = client.publish(&options, u.buf[..u.len].into()).await {
                    warn!("mqtt: publish: {e:?}");
                    break;
                }
            }
            Either4::Third(()) => {
                if let Err(e) = client.ping().await {
                    warn!("mqtt: ping: {e:?}");
                    break;
                }
            }
            Either4::Fourth(()) => {
                let n = stats(gateway_id, &mut buf);
                if let Err(e) = client.publish(&stats_options, buf[..n].into()).await {
                    warn!("mqtt: stats: {e:?}");
                    break;
                }
            }
        }
    }
    // Dropping the client closes the socket; the broker then publishes the will.
    Ok(())
}

/// Encodes a `GatewayStats` message with the duty-cycle load into `buf`. The RX
/// counters stay zero until there
/// is a radio.
fn stats(gateway_id: &str, buf: &mut [u8]) -> usize {
    let mut build = heapless::String::<32>::new();
    let _ = write!(build, "{}", Utc(config::build_time()));
    let metadata = [("firmware", env!("CARGO_PKG_VERSION")), ("build", build.as_str())];
    // The session only runs once the clock is set.
    let time_us = clock::now_unix_us().unwrap_or(0);
    let (tx_received, tx_emitted) = downlink::take_counters();
    let counters = chirpstack::Counters { tx_received, tx_emitted, ..Default::default() };
    let used = downlink::duty_cycle_used();
    let bands: [_; EU868.len()] = core::array::from_fn(|i| chirpstack::DutyBand {
        name: EU868[i].name,
        frequency_min: *EU868[i].hz.start(),
        frequency_max: *EU868[i].hz.end(),
        load_max_us: EU868[i].limit_us(),
        load_tracked_us: used[i],
    });
    let duty = chirpstack::DutyCycle { window_us: duty::WINDOW_US, bands: &bands };
    chirpstack::gateway_stats(gateway_id, time_us, &counters, Some(&duty), &metadata, buf).unwrap_or(0)
}
