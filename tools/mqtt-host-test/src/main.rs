//! Host test: rust-mqtt 0.6 over embedded-tls (main) against the test broker from
//! tools/test-broker (Mosquitto, tested with 2.0.11). Run after bumping rust-mqtt or the
//! embedded-tls rev:
//!
//!     tools/test-broker/setup.sh && mosquitto -c tools/test-broker/out/mosquitto.conf
//!     cd tools/mqtt-host-test && cargo run --release
//!
//! `CHOP=3` makes the transport return 1-3 bytes per read, so the select! test cancels
//! TLS reads in the middle of records.
//!
//! Stack and options as planned for the gateway: TLS 1.3, P-256, rustpki verifier with a
//! fail-closed clock wrapper, full-size TLS read buffer, no allocator (BumpBuffer), MQTT 5, user/password,
//! retained Last Will on state/conn, QoS 1.

use std::num::NonZero;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use embassy_crypto_rand as _;
use embassy_crypto_rustcrypto as _;
use embedded_io_adapters::tokio_1::FromTokio;
use embedded_tls::pki::CertVerifier;
use embedded_tls::{
    Aes128GcmSha256, Certificate, CertificateRef, CertificateVerifyRef,
    TlsClock, TlsConfig, TlsConnection, TlsContext, TlsError, TlsVerifier,
};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::event::Event;
use rust_mqtt::client::options::{
    ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference, WillOptions,
};
use rust_mqtt::config::KeepAlive;
use rust_mqtt::types::{MqttBinary, MqttString, TopicFilter, TopicName};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

const BROKER: &str = "127.0.0.1:8883";
const HOST: &str = "mqtt.test";
const GW: &str = "0016c001ff10a235";
const PW: &str = "secret-pw";

type Suite = Aes128GcmSha256;
type Tls<'a> = TlsConnection<'a, Choppy<FromTokio<TcpStream>>, Suite>;

/// Transport below TLS that returns at most CHOP bytes per read and yields before each read,
/// so a select! can cancel the TLS read in the middle of a record header or body.
struct Choppy<T> {
    inner: T,
    chop: usize,
    n: usize,
}

impl<T: embedded_io_async::ErrorType> embedded_io_async::ErrorType for Choppy<T> {
    type Error = T::Error;
}

impl<T: embedded_io_async::Read> embedded_io_async::Read for Choppy<T> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if self.chop < usize::MAX {
            tokio::task::yield_now().await;
        }
        self.n = self.n.wrapping_add(1);
        let max = if self.chop == usize::MAX { buf.len() } else { 1 + self.n % self.chop };
        let len = buf.len().min(max);
        self.inner.read(&mut buf[..len]).await
    }
}

impl<T: embedded_io_async::Write> embedded_io_async::Write for Choppy<T> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.inner.write(buf).await
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().await
    }
}
// SUBSCRIBE_MAXIMUM, RECEIVE_MAXIMUM, SEND_MAXIMUM, MAX_SUBSCRIPTION_IDENTIFIERS,
// MAX_USER_PROPERTIES, MAX_INCOMING_TOPIC_ALIASES, MAX_OUTGOING_TOPIC_ALIASES
type Mqtt<'a, 'c> = Client<'a, 'c, Tls<'c>, BumpBuffer<'c>, 2, 4, 4, 0, 0, 0, 0>;

fn dir() -> String {
    std::env::var("BROKER_DIR")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../test-broker/out").into())
}

struct SysClock;
impl TlsClock for SysClock {
    fn now() -> Option<u64> {
        Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs())
    }
}

struct NoClock;
impl TlsClock for NoClock {
    fn now() -> Option<u64> {
        None
    }
}

/// rustpki skips the validity dates when the time is unknown; refuse instead (sketch section 6).
struct FailClosed<'a, C: TlsClock> {
    inner: CertVerifier<'a, Suite, C, 2048>,
}

impl<C: TlsClock> TlsVerifier<Suite> for FailClosed<'_, C> {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.inner.set_hostname_verification(hostname)
    }
    fn verify_certificate(
        &mut self,
        transcript: &<Suite as embedded_tls::TlsCipherSuite>::Hash,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        if C::now().is_none() {
            return Err(TlsError::InvalidCertificate);
        }
        self.inner.verify_certificate(transcript, cert)
    }
    fn verify_signature(&mut self, verify: CertificateVerifyRef) -> Result<(), TlsError> {
        self.inner.verify_signature(verify)
    }
}

async fn tls_open<'b, C: TlsClock>(
    ca: &[u8],
    host: &str,
    rx: &'b mut [u8],
    tx: &'b mut [u8],
) -> Result<Tls<'b>, TlsError> {
    let tcp = TcpStream::connect(BROKER).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    let config = TlsConfig::new()
        .with_server_name(host);
    let chop = std::env::var("CHOP").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let mut tls = TlsConnection::new(Choppy { inner: FromTokio::new(tcp), chop, n: 0 }, rx, tx);
    let verifier = FailClosed::<C> {
        inner: CertVerifier::new(Certificate::X509(ca)),
    };
    tls.open(TlsContext::new(&config, verifier)).await?;
    Ok(tls)
}

fn check(name: &str, ok: bool, detail: impl std::fmt::Debug) -> bool {
    println!("[{}] {name}: {detail:?}", if ok { "PASS" } else { "FAIL" });
    ok
}

fn topic(t: &str) -> TopicName<'_> {
    TopicName::new(MqttString::from_str(t).unwrap()).unwrap()
}

/// Runs mosquitto_sub as admin and returns its output lines (topic|retain|qos|len|hex payload).
async fn admin_sub(filter: &str, count: usize, wait_s: u32) -> tokio::task::JoinHandle<Vec<String>> {
    let d = dir();
    let mut child = Command::new("mosquitto_sub")
        .args(["-h", "127.0.0.1", "-p", "8883", "--cafile", &format!("{d}/ca.pem"), "--insecure"])
        .args(["--tls-version", "tlsv1.3", "-u", "admin", "-P", "admin-pw", "-V", "mqttv5"])
        .args(["-t", filter, "-v", "-F", "%t|%r|%q|%l|%x"])
        .args(["-C", &count.to_string(), "-W", &wait_s.to_string()])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let h = tokio::spawn(async move {
        let mut lines = BufReader::new(out).lines();
        let mut v = Vec::new();
        while let Ok(Some(l)) = lines.next_line().await {
            v.push(l);
        }
        let _ = child.wait().await;
        v
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    h
}

async fn admin_pub(topic: &str, file: &str, qos: &str, repeat: u32) {
    let d = dir();
    let st = Command::new("mosquitto_pub")
        .args(["-h", "127.0.0.1", "-p", "8883", "--cafile", &format!("{d}/ca.pem"), "--insecure"])
        .args(["--tls-version", "tlsv1.3", "-u", "admin", "-P", "admin-pw", "-V", "mqttv5"])
        .args(["-t", topic, "-q", qos, "-f", file])
        .args(["--repeat", &repeat.to_string(), "--repeat-delay", "0"])
        .status()
        .await
        .unwrap();
    assert!(st.success());
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

/// Polls until a matching event arrives; returns false on timeout.
async fn wait_for(
    client: &mut Mqtt<'_, '_>,
    mut pred: impl FnMut(&Event<'_, 0, 0>) -> bool,
    timeout: Duration,
) -> bool {
    let fut = async {
        loop {
            let ok = match client.poll().await {
                Ok(e) => {
                    log::debug!("event {e:?}");
                    pred(&e)
                }
                Err(e) => {
                    println!("  poll error: {e:?}");
                    return false;
                }
            };
            // Safety: the event (which borrows the bump buffer) was dropped above.
            unsafe { client.buffer_mut().reset() };
            if ok {
                return true;
            }
        }
    };
    tokio::time::timeout(timeout, fut).await.unwrap_or(false)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let d = dir();
    let ca = std::fs::read(format!("{d}/ca.der")).unwrap();
    let other_ca = std::fs::read(format!("{d}/other-ca.der")).unwrap();
    let mut all = true;

    // Same sizes as the firmware (fw/src/mqtt.rs).
    let mut rx = [0u8; 16640];
    let mut tx = [0u8; 4352];

    // --- TLS negative tests ---------------------------------------------------------------
    let r = tls_open::<SysClock>(&other_ca, HOST, &mut rx, &mut tx).await.map(|_| ());
    all &= check("TLS refuses unknown CA", matches!(r, Err(TlsError::InvalidCertificate)), r);
    let r = tls_open::<SysClock>(&ca, "wrong.test", &mut rx, &mut tx).await.map(|_| ());
    all &= check("TLS refuses wrong hostname", matches!(r, Err(TlsError::InvalidCertificate)), r);
    let r = tls_open::<NoClock>(&ca, HOST, &mut rx, &mut tx).await.map(|_| ());
    all &= check("TLS fails closed without time", matches!(r, Err(TlsError::InvalidCertificate)), r);

    // --- wrong password ---------------------------------------------------------------------
    {
        let tls = tls_open::<SysClock>(&ca, HOST, &mut rx, &mut tx).await.unwrap();
        let mut mem = [0u8; 1024];
        let mut bump = BumpBuffer::new(&mut mem);
        let mut client: Mqtt = Client::new(&mut bump);
        let opts = ConnectOptions::new()
            .clean_start()
            .user_name(MqttString::from_str(GW).unwrap())
            .password(MqttBinary::try_from(b"wrong".as_slice()).unwrap());
        let r = client.connect(tls, &opts, Some(MqttString::from_str(GW).unwrap())).await;
        all &= check("MQTT refuses wrong password", r.is_err(), r.map(|_| ()));
    }

    // --- main session -------------------------------------------------------------------
    let state_topic = format!("eu868/gateway/{GW}/state/conn");
    let up_topic = format!("eu868/gateway/{GW}/event/up");
    let down_topic = format!("eu868/gateway/{GW}/command/down");
    let foreign_topic = "eu868/gateway/ffffffffffffffff/event/up";

    // admin watches all of this gateway's events and state
    let watch = admin_sub(&format!("eu868/gateway/{GW}/#"), 1000, 25).await;

    let t0 = Instant::now();
    let tls = tls_open::<SysClock>(&ca, HOST, &mut rx, &mut tx).await.unwrap();
    let handshake = t0.elapsed();

    let mut mem = vec![0u8; 16 * 1024];
    let mut bump = BumpBuffer::new(&mut mem);
    let mut client: Mqtt = Client::new(&mut bump);

    let will = WillOptions::new(topic(&state_topic), MqttBinary::try_from(b"OFFLINE".as_slice()).unwrap())
        .at_least_once()
        .retain();
    let opts = ConnectOptions::new()
        .clean_start()
        .keep_alive(KeepAlive::Seconds(NonZero::new(30).unwrap()))
        .maximum_packet_size(NonZero::new(12 * 1024).unwrap())
        .user_name(MqttString::from_str(GW).unwrap())
        .password(MqttBinary::try_from(PW.as_bytes()).unwrap())
        .will(will);
    let r = client.connect(tls, &opts, Some(MqttString::from_str(GW).unwrap())).await;
    all &= check("MQTT connect (TLS 1.3, user/pw, will)", r.is_ok(), (handshake, r.as_ref().map(|_| ()).map_err(|e| format!("{e:?}"))));
    drop(r);
    unsafe { client.buffer_mut().reset() };

    // retained ONLINE, QoS 1
    let st = topic(&state_topic);
    let pid = client
        .publish(&PublicationOptions::new(TopicReference::Name(st.as_borrowed())).at_least_once().retain(), b"ONLINE".as_slice().into())
        .await;
    let acked = wait_for(&mut client, |e| matches!(e, Event::PublishAcknowledged(p) if p.reason_code.is_success()), Duration::from_secs(5)).await;
    all &= check("publish retained state/conn QoS1, PUBACK", pid.is_ok() && acked, pid.map_err(|e| format!("{e:?}")));

    // subscribe to command/down
    let filter = TopicFilter::new(MqttString::from_str(&down_topic).unwrap()).unwrap();
    let r = client.subscribe(filter, &SubscriptionOptions::new().at_least_once()).await;
    let subacked = wait_for(&mut client, |e| matches!(e, Event::Suback(s) if s.reason_code.is_success()), Duration::from_secs(5)).await;
    all &= check("subscribe command/down QoS1, SUBACK", r.is_ok() && subacked, r.map_err(|e| format!("{e:?}")));

    // uplinks QoS 0 and QoS 1
    let up = topic(&up_topic);
    let payload = pattern(256, 1);
    let r0 = client.publish(&PublicationOptions::new(TopicReference::Name(up.as_borrowed())), payload.as_slice().into()).await;
    let r1 = client.publish(&PublicationOptions::new(TopicReference::Name(up.as_borrowed())).at_least_once(), payload.as_slice().into()).await;
    let acked = wait_for(&mut client, |e| matches!(e, Event::PublishAcknowledged(p) if p.reason_code.is_success()), Duration::from_secs(5)).await;
    all &= check("publish event/up QoS0 + QoS1 (256 B)", r0.is_ok() && r1.is_ok() && acked, (r0.is_ok(), r1.is_ok(), acked));

    // ACL: publishing to another gateway's topic must be refused
    let ft = topic(foreign_topic);
    let _ = client.publish(&PublicationOptions::new(TopicReference::Name(ft.as_borrowed())).at_least_once(), b"x".as_slice().into()).await;
    let mut reason = String::new();
    let got = wait_for(
        &mut client,
        |e| match e {
            Event::PublishAcknowledged(p) => { reason = format!("PUBACK {:?}", p.reason_code); true }
            Event::PublishRejected(p) => { reason = format!("rejected {:?}", p.reason_code); true }
            _ => false,
        },
        Duration::from_secs(5),
    )
    .await;
    all &= check("ACL: foreign topic refused", got && reason.starts_with("rejected"), &reason);

    // downlinks from the broker: 1 KB and 10 KB (one TLS record of ~10 KB)
    for len in [1024usize, 10 * 1024] {
        let data = pattern(len, len as u8);
        let file = format!("{d}/down-{len}.bin");
        std::fs::write(&file, &data).unwrap();
        let (dt, df) = (down_topic.clone(), file.clone());
        let p = tokio::spawn(async move { admin_pub(&dt, &df, "1", 1).await });
        let mut ok_payload = false;
        let got = wait_for(&mut client, |e| match e {
            Event::Publish(p) => { ok_payload = p.message.as_ref() == data.as_slice(); true }
            _ => false,
        }, Duration::from_secs(10)).await;
        p.await.unwrap();
        all &= check(&format!("receive {len} B downlink QoS1"), got && ok_payload, (got, ok_payload));
    }

    // over the client's maximum packet size (12 KB): broker must not deliver it
    {
        let file = format!("{d}/down-big.bin");
        std::fs::write(&file, pattern(20 * 1024, 7)).unwrap();
        let dt = down_topic.clone();
        admin_pub(&dt, &file, "1", 1).await;
        let got = wait_for(&mut client, |e| matches!(e, Event::Publish(_)), Duration::from_secs(2)).await;
        all &= check("20 KB downlink above maximum_packet_size not delivered", !got, got);
        // the timeout above cancelled poll(): only safe here because nothing was in flight
    }

    // full duplex: poll_header vs. periodic uplinks in select!, while the broker floods downlinks
    {
        let n_down = 300u32;
        let data = pattern(1500, 3);
        let file = format!("{d}/down-1500.bin");
        std::fs::write(&file, &data).unwrap();
        let dt = down_topic.clone();
        let flood = tokio::spawn(async move { admin_pub(&dt, &file, "1", n_down).await });
        let mut tick = tokio::time::interval(Duration::from_millis(2));
        let (mut rx_ok, mut rx_bad, mut tx_n, mut cancelled) = (0u32, 0u32, 0u32, 0u32);
        let up_payload = pattern(200, 9);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut err = None;
        while rx_ok + rx_bad < n_down && Instant::now() < deadline {
            tokio::select! {
                h = client.poll_header() => {
                    let h = match h { Ok(h) => h, Err(e) => { err = Some(format!("{e:?}")); break } };
                    match client.poll_body(h).await {
                        Ok(Event::Publish(p)) => if p.message.as_ref() == data.as_slice() { rx_ok += 1 } else { rx_bad += 1 },
                        Ok(_) => {}
                        Err(e) => { err = Some(format!("{e:?}")); break }
                    }
                    unsafe { client.buffer_mut().reset() };
                }
                _ = tick.tick() => {
                    cancelled += 1;
                    match client.publish(&PublicationOptions::new(TopicReference::Name(up.as_borrowed())), up_payload.as_slice().into()).await {
                        Ok(_) => tx_n += 1,
                        Err(e) => { err = Some(format!("{e:?}")); break }
                    }
                }
            }
        }
        let _ = flood.await;
        all &= check(
            "select!: poll_header cancelled by uplinks, 300 x 1500 B downlinks",
            err.is_none() && rx_ok == n_down && rx_bad == 0,
            format!("rx ok {rx_ok}, bad {rx_bad}, uplinks sent {tx_n}, poll_header cancelled {cancelled}x, error {err:?}"),
        );
        UPLINKS_FLOOD.store(tx_n, std::sync::atomic::Ordering::Relaxed);
    }

    // keepalive ping
    let r = client.ping().await;
    let pong = wait_for(&mut client, |e| matches!(e, Event::Pingresp), Duration::from_secs(5)).await;
    all &= check("PINGREQ / PINGRESP", r.is_ok() && pong, pong);

    // drop the connection without DISCONNECT: broker must publish the retained will
    let net = client.abort().await;
    drop(net);
    drop(client);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let lines = watch.await.unwrap();
    let ups = lines.iter().filter(|l| l.starts_with(&up_topic)).count() as u32;
    let expected_ups = 2 + UPLINKS_FLOOD.load(std::sync::atomic::Ordering::Relaxed);
    all &= check("admin saw all uplinks", ups == expected_ups, format!("{ups} of {expected_ups}"));
    let last_state = lines.iter().filter(|l| l.starts_with(&state_topic)).last().cloned();
    all &= check("last state/conn = OFFLINE (will)", last_state.as_deref().is_some_and(|l| l.ends_with("|4f46464c494e45")), &last_state);

    // a fresh subscriber gets the retained OFFLINE
    let retained = admin_sub(&state_topic, 1, 3).await.await.unwrap();
    all &= check("state/conn OFFLINE is retained", retained.first().is_some_and(|l| l.contains("|1|0|7|4f46464c494e45")), &retained);

    println!(
        "sizes (host, 64-bit): Client {} B, TlsConnection {} B, FailClosed verifier {} B, TLS buffers 16640 + 4352 B",
        std::mem::size_of::<Mqtt<'static, 'static>>(),
        std::mem::size_of::<Tls<'static>>(),
        std::mem::size_of::<FailClosed<'static, SysClock>>(),
    );
    println!("{}", if all { "ALL PASS" } else { "SOME FAILED" });
    std::process::exit(if all { 0 } else { 1 });
}

static UPLINKS_FLOOD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
