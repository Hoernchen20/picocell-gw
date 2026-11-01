//! MQTT task: TCP → TLS → MQTT 5 session with ChirpStack's topics (sketch.md section 3.3).
//!
//! One task owns the connection and selects between incoming packets (downlinks, each
//! answered with a `DownlinkTxAck`), queued uplinks, stats and keepalive pings. Only `poll_header()` is cancel-safe, so it is the one that
//! sits in the select; `poll_body()` always runs to completion.
//!
//! Remote password change (sketch.md section 5.2): the raw new password published to
//! `command/credentials` (not retained) is stored next to the current one and used
//! from the next connection on, typically once the broker has switched to it and
//! disconnected the gateway (Mosquitto does on reload). Accepted by the broker, it
//! replaces the stored one and the gateway restarts; refused, the next attempt uses
//! the stored one, and when that is accepted the new one is dropped. Outcomes go to
//! `state/credentials` as text.
//!
//! Firmware update (sketch.md section 8.1, 8b): messages on `command/ota` go to
//! `ota::Receiver`, answers to `state/ota`. An OTA trial image is bought once the
//! broker has granted all subscriptions, so it can take the next update.
//!
//! CA rotation (sketch.md section 8.1, 8e): signed updates on `command/ca` go to
//! `ca_update`, answers to `state/ca`. A session that only came up with `tls.ca1`
//! makes it `tls.ca0` and restarts.

use core::cell::Cell;
use core::fmt::Write;
use core::num::NonZero;
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer, with_timeout};
use embedded_tls::{TlsConfig, TlsConnection, TlsContext};
use log::{info, warn};
use jit::duty::{self, EU868};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::{Client, MqttError};
use rust_mqtt::client::event::Event;
use rust_mqtt::client::options::{
    ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference, WillOptions,
};
use rust_mqtt::config::KeepAlive;
use rust_mqtt::types::{MqttBinary, MqttString, ReasonCode, TopicFilter, TopicName};

use wire::time::Utc;

use crate::tls::{self, FailClosed, Suite};
use crate::{boot, ca_update, clock, config, config_key, downlink, ota, radio, resolve};

const KEEP_ALIVE_S: u16 = 30;
const MAX_PACKET: u32 = 8192;
/// ChirpStack's default `stats_interval`.
const STATS_INTERVAL_S: u64 = 30;
/// Reconnect backoff: doubles from 1 s up to this, plus up to a quarter at random so
/// gateways do not all come back at once after a broker restart.
const BACKOFF_MAX_S: u64 = 60;
/// A session at least this long resets the backoff; a shorter one (broker accepts and
/// drops again, e.g. an ACL) keeps it growing.
const STABLE_S: u64 = 60;
/// One SUBSCRIBE each for `command/down`, `command/credentials`, `command/ota`,
/// `command/ca`.
const SUBSCRIPTIONS: usize = 4;

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
    session: SessionBuffers,
}

/// The ones above the socket, borrowed by `run()` while `session()` keeps the socket.
struct SessionBuffers {
    tls_rx: [u8; TLS_RX],
    tls_tx: [u8; 4352],
    mqtt: [u8; MAX_PACKET as usize],
}

impl Buffers {
    pub const fn new() -> Self {
        Self {
            tcp_rx: [0; 4352],
            tcp_tx: [0; 4352],
            session: SessionBuffers {
                tls_rx: [0; TLS_RX],
                tls_tx: [0; 4352],
                mqtt: [0; MAX_PACKET as usize],
            },
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

/// Disabled until the task starts, which it does not in provisioning mode or on a
/// board that is not locked (production builds).
static STATE: AtomicU8 = AtomicU8::new(State::Disabled as u8);

fn set_state(s: State) {
    STATE.store(s as u8, Ordering::Relaxed);
}

/// The connection state in words.
pub fn state() -> &'static str {
    match STATE.load(Ordering::Relaxed) {
        x if x == State::Disabled as u8 => "disabled (offline)",
        x if x == State::WaitNetwork as u8 => "waiting for the network",
        x if x == State::WaitTime as u8 => "waiting for the time",
        x if x == State::Connecting as u8 => "connecting",
        x if x == State::Connected as u8 => "connected",
        _ => "disconnected, retrying",
    }
}

/// For the status LED.
pub fn connected() -> bool {
    STATE.load(Ordering::Relaxed) == State::Connected as u8
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
    // CA slot to verify the broker with; after it rejected the broker's certificate the
    // other one, if set (CA rotation, sketch.md section 5.3). Other handshake failures
    // keep it, so disrupting handshakes cannot force the fallback.
    let mut ca_slot = 0;
    // While a new password is staged, attempts alternate between it and the stored one
    // until the broker accepts either; the new one first.
    let mut use_new = config::staged_password().is_some();
    // Survives reconnects: the sender resends the chunk the gateway did not answer.
    let mut ota = ota::Receiver::new();
    loop {
        set_state(State::WaitNetwork);
        // A static address stays configured without a link.
        stack.wait_link_up().await;
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
        let new = if use_new { config::staged_password() } else { None };
        let start = Instant::now();
        match session(stack, port, ca_slot, new.as_deref(), gateway_id, bufs, &mut ota).await {
            // Was connected: retry with a password staged meanwhile, soon if it was up
            // for a while.
            Ok(()) => {
                if start.elapsed() >= Duration::from_secs(STABLE_S) {
                    backoff = 1;
                }
                use_new = config::staged_password().is_some();
            }
            Err(e) => {
                warn!("mqtt: {e}");
                if e == CERT_REJECTED && !config::get().ca(1 - ca_slot).is_empty() {
                    ca_slot = 1 - ca_slot;
                    info!("mqtt: trying tls.ca{ca_slot}");
                }
                if e == REFUSED && config::staged_password().is_some() {
                    use_new = !use_new;
                    info!("mqtt: trying the {} password", if use_new { "new" } else { "stored" });
                }
            }
        }
        set_state(State::Backoff);
        let mut r = [0u8; 2];
        tls::random_bytes(&mut r);
        let wait_ms = backoff * 1000 + u64::from(u16::from_le_bytes(r)) * backoff * 250 / 65536;
        info!("mqtt: reconnecting in {} ms", wait_ms);
        Timer::after_millis(wait_ms).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_S);
    }
}

const TLS_FAILED: &str = "tls handshake failed";
const CERT_REJECTED: &str = "tls handshake failed: broker certificate rejected";
const REFUSED: &str = "broker refused the password";

/// A message on `command/credentials`: the new password, as it is.
fn new_password(retain: bool, payload: &[u8]) -> Result<config::Password, &'static str> {
    // A retained one would stay on the broker.
    if retain {
        return Err("retained message refused");
    }
    let pw = core::str::from_utf8(payload).map_err(|_| "not text")?;
    config::check_password(pw)?;
    Ok(config::Password::try_from(pw).unwrap())
}

/// What an incoming publication asks for once the buffer is free again.
enum Incoming {
    Nothing,
    Ack(usize),
    Password(Result<config::Password, &'static str>),
    /// The answer for `state/ota`, and whether to reboot into the new image after it.
    Ota(ota::Reply, bool),
    /// The answer for `state/ca`, and whether to restart after it.
    Ca(ca_update::Reply, bool),
}

/// One connection, with the staged password `new` or else the stored one. `Ok` once
/// the session was up and then ended, `Err` if it never came up.
async fn session(
    stack: Stack<'static>,
    port: u16,
    ca_slot: usize,
    new: Option<&str>,
    gateway_id: &str,
    bufs: &mut Buffers,
    ota: &mut ota::Receiver,
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

    let mut socket = TcpSocket::new(stack, &mut bufs.tcp_rx, &mut bufs.tcp_tx);
    // Also bounds poll_body(): never wrap MQTT reads in a timeout (not cancel-safe).
    socket.set_timeout(Some(Duration::from_secs(u64::from(KEEP_ALIVE_S) * 3)));
    socket.connect((addr, port)).await.map_err(|_| "tcp connect failed")?;
    let result = run(&mut socket, stack, ca_slot, new, gateway_id, &mut bufs.session, ota).await;
    // Dropping the socket sends nothing, and the broker would keep the session (and
    // hold back the will) until the keep-alive runs out or the gateway is back. A
    // reset ends it now; the will goes out as no DISCONNECT came first.
    socket.abort();
    let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
    result
}

/// The TLS and MQTT session on the connected `socket`; returns when it ends.
async fn run(
    socket: &mut TcpSocket<'_>,
    stack: Stack<'static>,
    ca_slot: usize,
    new: Option<&str>,
    gateway_id: &str,
    bufs: &mut SessionBuffers,
    ota: &mut ota::Receiver,
) -> Result<(), &'static str> {
    let cfg = config::get();
    let host = cfg.mqtt_host();
    let state_topic = topic(gateway_id, "state/conn");
    let up_topic = topic(gateway_id, "event/up");
    let stats_topic = topic(gateway_id, "event/stats");
    let ack_topic = topic(gateway_id, "event/ack");
    let down_topic = topic(gateway_id, "command/down");
    let password_topic = topic(gateway_id, "command/credentials");
    let report_topic = topic(gateway_id, "state/credentials");
    let ota_topic = topic(gateway_id, "command/ota");
    let ota_report_topic = topic(gateway_id, "state/ota");
    let ca_topic = topic(gateway_id, "command/ca");
    let ca_report_topic = topic(gateway_id, "state/ca");

    let tls_config = TlsConfig::new().with_server_name(host);
    let mut tls: TlsConnection<'_, _, Suite> =
        TlsConnection::new(socket, &mut bufs.tls_rx, &mut bufs.tls_tx);
    let rejected = Cell::new(false);
    tls.open(TlsContext::new(&tls_config, FailClosed::new(cfg.ca(ca_slot), &rejected)))
        .await
        .map_err(|_| if rejected.get() { CERT_REJECTED } else { TLS_FAILED })?;

    let mut bump = BumpBuffer::new(&mut bufs.mqtt);
    // The first 4: SUBSCRIBEs awaiting their SUBACK, one per subscription below.
    let mut client: Client<'_, '_, _, _, 4, 4, 4, 0, 0, 0, 0> = Client::new(&mut bump);
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
        .password(MqttBinary::try_from(new.unwrap_or(cfg.mqtt_password()).as_bytes()).unwrap())
        .will(will);
    client
        .connect(tls, &options, Some(MqttString::from_str(gateway_id).unwrap()))
        .await
        .map_err(|e| match e {
            MqttError::Disconnect {
                reason: ReasonCode::BadUserNameOrPassword | ReasonCode::NotAuthorized,
                ..
            } => REFUSED,
            _ => "mqtt connect refused",
        })?;
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

    let report_options = PublicationOptions::new(TopicReference::Name(name(&report_topic)));
    // Settle a staged password: the broker accepted the new one, or (after refusing
    // it) the stored one.
    if config::staged_password().is_some() {
        if new.is_some() {
            match config::commit_password().await {
                Ok(()) => {
                    info!("mqtt: new password accepted and stored, restarting");
                    let _ = client.publish(&report_options, b"committed"[..].into()).await;
                    Timer::after_millis(200).await;
                    cortex_m::peripheral::SCB::sys_reset();
                }
                // Still staged: the next connection tries again.
                Err(e) => warn!("mqtt: storing the new password: {e}"),
            }
        } else {
            warn!("mqtt: broker refused the new password, keeping the stored one");
            if let Err(e) = config::drop_password().await {
                warn!("mqtt: dropping the new password: {e}");
            }
            let msg = b"rejected: broker refused it, the stored password stays";
            if client.publish(&report_options, msg[..].into()).await.is_err() {
                return Ok(());
            }
        }
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
                // So a retained downlink shows as one and can be dropped.
                &SubscriptionOptions::new().at_least_once().retain_as_published(),
            )
            .await
            .is_ok()
        && client
            .subscribe(
                TopicFilter::new(MqttString::from_str(password_topic.as_str()).unwrap()).unwrap(),
                // Otherwise the broker clears the retain flag on live messages, and a
                // retained password would go unnoticed.
                &SubscriptionOptions::new().at_least_once().retain_as_published(),
            )
            .await
            .is_ok()
        && client
            .subscribe(
                TopicFilter::new(MqttString::from_str(ota_topic.as_str()).unwrap()).unwrap(),
                &SubscriptionOptions::new().at_least_once().retain_as_published(),
            )
            .await
            .is_ok()
        // Signed, so a retained update is fine: it reaches gateways that were offline.
        && client
            .subscribe(
                TopicFilter::new(MqttString::from_str(ca_topic.as_str()).unwrap()).unwrap(),
                &SubscriptionOptions::new().at_least_once(),
            )
            .await
            .is_ok();
    if !ok {
        return Err("publish/subscribe failed");
    }

    let ca_options = PublicationOptions::new(TopicReference::Name(name(&ca_report_topic)));
    // Only tried after tls.ca0 rejected the broker's certificate: the broker has moved
    // to the new CA, so the old one goes.
    if ca_slot == 1 {
        match config::promote_ca().await {
            Ok(()) => {
                info!("mqtt: broker only reachable with tls.ca1, now tls.ca0; restarting");
                let msg = b"promoted: tls.ca1 is now tls.ca0, restarting";
                let _ = client.publish(&ca_options, msg[..].into()).await;
                Timer::after_millis(200).await;
                cortex_m::peripheral::SCB::sys_reset();
            }
            Err(e) => warn!("mqtt: promoting tls.ca1: {e}"),
        }
    }

    let stats_options = PublicationOptions::new(TopicReference::Name(name(&stats_topic)));
    let ack_options = PublicationOptions::new(TopicReference::Name(name(&ack_topic)));
    let ota_options = PublicationOptions::new(TopicReference::Name(name(&ota_report_topic)));
    let mut buf = [0u8; 512];
    // Right away, so ChirpStack shows the gateway online without waiting an interval.
    let n = stats(gateway_id, &mut buf);
    if let Err(e) = client.publish(&stats_options, buf[..n].into()).await {
        warn!("mqtt: stats: {e:?}");
        return Ok(());
    }

    let mut ping = Ticker::every(Duration::from_secs(u64::from(KEEP_ALIVE_S) * 2 / 3));
    // A PINGREQ without PINGRESP by the next ping ends the session (dead broker or path
    // that TCP alone would only notice after its timeout).
    let mut ping_pending = false;
    let mut stats_ticker = Ticker::every(Duration::from_secs(STATS_INTERVAL_S));
    // Granted SUBACKs; the OTA trial image is bought once all are in.
    let mut granted = 0;
    loop {
        // A pulled cable ends the session right away instead of after the TCP timeout.
        let ping_or_link = select(ping.next(), stack.wait_link_down());
        match select4(client.poll_header(), UPLINKS.receive(), ping_or_link, stats_ticker.next()).await {
            Either4::First(Ok(header)) => {
                let incoming = match client.poll_body(header).await {
                    Ok(Event::Publish(p)) => match p.topic.name().map(|t| t.as_ref().as_str()) {
                        Some(t) if t == password_topic.as_str() => {
                            Incoming::Password(new_password(p.retain, p.message.as_ref()))
                        }
                        // Flash writes straight from the receive buffer.
                        Some(t) if t == ota_topic.as_str() => {
                            let (reply, reboot) = ota.handle(p.retain, p.message.as_ref()).await;
                            Incoming::Ota(reply, reboot)
                        }
                        Some(t) if t == ca_topic.as_str() => {
                            let (reply, restart) = ca_update::handle(p.message.as_ref()).await;
                            Incoming::Ca(reply, restart)
                        }
                        // It would be sent again on every connection.
                        _ if p.retain => {
                            warn!("mqtt: retained downlink ignored");
                            Incoming::Nothing
                        }
                        _ => match downlink::handle(gateway_id, p.message.as_ref(), &mut buf) {
                            Some(n) => Incoming::Ack(n),
                            None => Incoming::Nothing,
                        },
                    },
                    // A refused subscription would leave the gateway online but deaf,
                    // e.g. to downlinks: reconnect (with backoff) until it is granted.
                    Ok(Event::Suback(s)) if !s.reason_code.is_success() => {
                        warn!("mqtt: subscription refused: {:?}", s.reason_code);
                        break;
                    }
                    Ok(Event::Suback(_)) => {
                        granted += 1;
                        if granted == SUBSCRIPTIONS {
                            boot::confirm();
                        }
                        Incoming::Nothing
                    }
                    Ok(Event::PublishRejected(p)) => {
                        warn!("mqtt: publication refused: {:?}", p.reason_code);
                        Incoming::Nothing
                    }
                    Ok(Event::Pingresp) => {
                        ping_pending = false;
                        Incoming::Nothing
                    }
                    Ok(_) => Incoming::Nothing,
                    Err(e) => {
                        warn!("mqtt: {e:?}");
                        break;
                    }
                };
                // Safety: the event borrowing the buffer was dropped.
                unsafe { client.buffer_mut().reset() };
                let result = match incoming {
                    Incoming::Nothing => Ok(()),
                    Incoming::Ack(n) => client.publish(&ack_options, buf[..n].into()).await.map(|_| ()),
                    Incoming::Password(pw) => {
                        let staged = match pw {
                            Ok(pw) => config::stage_password(&pw).await,
                            Err(e) => Err(e),
                        };
                        let mut msg = heapless::String::<96>::new();
                        match staged {
                            Ok(()) => {
                                info!("mqtt: new password staged for the next connection");
                                let _ = msg.push_str("stored");
                            }
                            Err(e) => {
                                warn!("mqtt: password change rejected: {e}");
                                let _ = write!(msg, "rejected: {e}");
                            }
                        }
                        client.publish(&report_options, msg.as_bytes().into()).await.map(|_| ())
                    }
                    Incoming::Ota(reply, reboot) => {
                        let r = client.publish(&ota_options, reply.as_bytes().into()).await.map(|_| ());
                        if reboot {
                            Timer::after_millis(200).await;
                            ota.reboot();
                        }
                        r
                    }
                    Incoming::Ca(reply, restart) => {
                        let r = client.publish(&ca_options, reply.as_bytes().into()).await.map(|_| ());
                        if restart {
                            Timer::after_millis(200).await;
                            cortex_m::peripheral::SCB::sys_reset();
                        }
                        r
                    }
                };
                if let Err(e) = result {
                    warn!("mqtt: publish: {e:?}");
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
            Either4::Third(Either::Second(())) => {
                warn!("mqtt: Ethernet link down");
                break;
            }
            Either4::Third(Either::First(())) => {
                if ping_pending {
                    warn!("mqtt: no PINGRESP from the broker");
                    break;
                }
                ping_pending = true;
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
    Ok(())
}

/// Encodes a `GatewayStats` message with the packet counters and the duty-cycle load
/// into `buf`.
fn stats(gateway_id: &str, buf: &mut [u8]) -> usize {
    let mut build = heapless::String::<32>::new();
    let _ = write!(build, "{}", Utc(config::build_time()));
    let sec = boot::security();
    let boot_keys = sec.boot_keys();
    let metadata = [
        ("firmware", env!("CARGO_PKG_VERSION")),
        ("build", build.as_str()),
        ("variant", boot::build()),
        ("secure_boot", sec.secure_boot()),
        ("debug", sec.debug()),
        ("boot_keys", boot_keys.as_str()),
        ("config_key", config_key::source()),
    ];
    // The session only runs once the clock is set.
    let time_us = clock::now_unix_us().unwrap_or(0);
    let (tx_received, tx_emitted) = downlink::take_counters();
    let (rx_received, rx_received_ok) = radio::take_counters();
    let counters = chirpstack::Counters { rx_received, rx_received_ok, tx_received, tx_emitted };
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
