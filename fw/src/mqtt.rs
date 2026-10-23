//! MQTT task: TCP → TLS → MQTT 5 session with ChirpStack's topics (sketch.md section 3.3).
//!
//! One task owns the connection and selects between incoming packets, queued uplinks
//! and keepalive pings. Only `poll_header()` is cancel-safe, so it is the one that
//! sits in the select; `poll_body()` always runs to completion.

use core::fmt::Write;
use core::num::NonZero;

use embassy_futures::select::{Either3, select3};
use embassy_net::tcp::TcpSocket;
use embassy_net::{Ipv4Address, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Ticker, Timer};
use embedded_tls::{TlsConfig, TlsConnection, TlsContext};
use log::{info, warn};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::event::Event;
use rust_mqtt::client::options::{
    ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference, WillOptions,
};
use rust_mqtt::config::KeepAlive;
use rust_mqtt::types::{MqttBinary, MqttString, TopicFilter, TopicName};

use crate::config;
use crate::tls::{FailClosed, Suite};

const KEEP_ALIVE_S: u16 = 30;
const MAX_PACKET: u32 = 8192;
/// Placeholder for the protobuf ConnState messages.
const ONLINE: &[u8] = b"ONLINE";
const OFFLINE: &[u8] = b"OFFLINE";

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

type Topic = heapless::String<64>;

fn topic(gateway_id: &str, suffix: &str) -> Topic {
    let mut t = Topic::new();
    let _ = write!(t, "{}/gateway/{}/{}", config::TOPIC_PREFIX, gateway_id, suffix);
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
    let Some(broker) = config::broker() else {
        warn!("invalid PICOCELL_MQTT_ADDR / PICOCELL_MQTT_PORT, MQTT disabled");
        loop {
            Timer::after_secs(3600).await;
        }
    };
    let mut backoff = 1u64;
    loop {
        stack.wait_config_up().await;
        match session(stack, broker, gateway_id, bufs).await {
            // Was connected: retry soon.
            Ok(()) => backoff = 1,
            Err(e) => warn!("mqtt: {e}"),
        }
        info!("mqtt: reconnecting in {backoff} s");
        Timer::after_secs(backoff).await;
        backoff = (backoff * 2).min(60);
    }
}

/// One connection. `Ok` once the session was up and then ended, `Err` if it never came up.
async fn session(
    stack: Stack<'static>,
    broker: (Ipv4Address, u16),
    gateway_id: &str,
    bufs: &mut Buffers,
) -> Result<(), &'static str> {
    let state_topic = topic(gateway_id, "state/conn");
    let up_topic = topic(gateway_id, "event/up");
    let down_topic = topic(gateway_id, "command/down");

    let mut socket = TcpSocket::new(stack, &mut bufs.tcp_rx, &mut bufs.tcp_tx);
    // Also bounds poll_body(): never wrap MQTT reads in a timeout (not cancel-safe).
    socket.set_timeout(Some(Duration::from_secs(u64::from(KEEP_ALIVE_S) * 3)));
    socket.connect(broker).await.map_err(|_| "tcp connect failed")?;

    let tls_config = TlsConfig::new().with_server_name(config::MQTT_HOST);
    let mut tls: TlsConnection<'_, _, Suite> =
        TlsConnection::new(socket, &mut bufs.tls_rx, &mut bufs.tls_tx);
    tls.open(TlsContext::new(&tls_config, FailClosed::new(config::CA_DER)))
        .await
        .map_err(|_| "tls handshake failed")?;

    let mut bump = BumpBuffer::new(&mut bufs.mqtt);
    let mut client: Client<'_, '_, _, _, 2, 4, 4, 0, 0, 0, 0> = Client::new(&mut bump);
    let will = WillOptions::new(name(&state_topic), MqttBinary::try_from(OFFLINE).unwrap())
        .at_least_once()
        .retain();
    let options = ConnectOptions::new()
        .clean_start()
        .keep_alive(KeepAlive::Seconds(NonZero::new(KEEP_ALIVE_S).unwrap()))
        .maximum_packet_size(NonZero::new(MAX_PACKET).unwrap())
        .user_name(MqttString::from_str(gateway_id).unwrap())
        .password(MqttBinary::try_from(config::MQTT_PASSWORD.as_bytes()).unwrap())
        .will(will);
    client
        .connect(tls, &options, Some(MqttString::from_str(gateway_id).unwrap()))
        .await
        .map_err(|_| "mqtt connect refused")?;
    // Safety: the CONNACK info borrowing the buffer was dropped.
    unsafe { client.buffer_mut().reset() };
    info!("mqtt: connected to {}", config::MQTT_HOST);

    let up = name(&up_topic);
    let ok = client
        .publish(
            &PublicationOptions::new(TopicReference::Name(name(&state_topic)))
                .at_least_once()
                .retain(),
            ONLINE.into(),
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

    let mut ping = Ticker::every(Duration::from_secs(u64::from(KEEP_ALIVE_S) * 2 / 3));
    loop {
        match select3(client.poll_header(), UPLINKS.receive(), ping.next()).await {
            Either3::First(Ok(header)) => {
                match client.poll_body(header).await {
                    Ok(Event::Publish(p)) => {
                        info!("mqtt: downlink {} B", p.message.as_ref().len())
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("mqtt: {e:?}");
                        break;
                    }
                }
                // Safety: the event borrowing the buffer was dropped.
                unsafe { client.buffer_mut().reset() };
            }
            Either3::First(Err(e)) => {
                warn!("mqtt: {e:?}");
                break;
            }
            Either3::Second(u) => {
                let options = PublicationOptions::new(TopicReference::Name(up.as_borrowed()));
                if let Err(e) = client.publish(&options, u.buf[..u.len].into()).await {
                    warn!("mqtt: publish: {e:?}");
                    break;
                }
            }
            Either3::Third(()) => {
                if let Err(e) = client.ping().await {
                    warn!("mqtt: ping: {e:?}");
                    break;
                }
            }
        }
    }
    // Dropping the client closes the socket; the broker then publishes the will.
    Ok(())
}
