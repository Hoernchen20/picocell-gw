//! SNTP task: sets the wall clock from up to three servers (sketch.md section 6).
//!
//! The MQTT task does not connect before the first result, and the TLS verifier
//! refuses certificates while the time is unknown.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use heapless::Vec;
use log::{info, warn};
use wire::sntp::{self, Sample};
use wire::time::Utc;

use crate::{clock, config, resolve, tls};

const TIMEOUT: Duration = Duration::from_secs(2);
const TRIES: usize = 2;
const INTERVAL_S: u64 = 3600;
/// Retry while the time is unknown; nothing connects meanwhile.
const RETRY_UNSET_S: u64 = 10;
const RETRY_SET_S: u64 = 300;

#[embassy_executor::task]
pub async fn sntp_task(stack: Stack<'static>) -> ! {
    loop {
        stack.wait_config_up().await;
        let wait = if sync(stack).await {
            INTERVAL_S
        } else if clock::is_set() {
            RETRY_SET_S
        } else {
            RETRY_UNSET_S
        };
        Timer::after_secs(wait).await;
    }
}

async fn sync(stack: Stack<'_>) -> bool {
    let servers = config::get().ntp_servers();
    let mut samples: Vec<Sample, 3> = Vec::new();
    for name in &servers {
        let addr = match resolve::resolve(stack, name).await {
            Ok(a) => a,
            Err(e) => {
                warn!("sntp: {name}: {e}");
                continue;
            }
        };
        match query(stack, addr).await {
            Ok(s) => {
                let _ = samples.push(s);
            }
            Err(e) => warn!("sntp: {name} ({addr}): {e}"),
        }
    }
    let n = samples.len();
    let Some(s) = sntp::combine(&mut samples) else {
        return false;
    };
    let now_us = s.base_us + Instant::now().as_micros() as i64;
    let now_s = u64::try_from(now_us / 1_000_000).unwrap_or(0);
    // A rogue or broken server must not move the clock back past the build, where
    // expired certificates would be valid again.
    if now_s < config::build_time() {
        warn!("sntp: server time {} is before the build, ignored", Utc(now_s));
        return false;
    }
    match clock::base_us() {
        None => info!("sntp: clock set to {} ({n} of {} servers)", Utc(now_s), servers.len()),
        Some(old) => info!(
            "sntp: clock corrected by {} ms ({n} of {} servers)",
            (s.base_us - old) / 1000,
            servers.len()
        ),
    }
    clock::set(s.base_us);
    true
}

async fn query(stack: Stack<'_>, server: Ipv4Address) -> Result<Sample, &'static str> {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buf = [0u8; 2 * sntp::LEN];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buf = [0u8; sntp::LEN];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(0).map_err(|_| "bind failed")?;
    let from = IpAddress::from(server);

    let mut err = "no reply";
    for _ in 0..TRIES {
        let mut cookie = [0u8; 8];
        tls::random_bytes(&mut cookie);
        let t1 = Instant::now().as_micros();
        socket.send_to(&sntp::request(cookie), (server, sntp::PORT)).await.map_err(|_| "send failed")?;
        let reply = with_timeout(TIMEOUT, async {
            let mut buf = [0u8; 2 * sntp::LEN];
            loop {
                let Ok((n, meta)) = socket.recv_from(&mut buf).await else {
                    continue;
                };
                let t4 = Instant::now().as_micros();
                if meta.endpoint.addr != from || meta.endpoint.port != sntp::PORT {
                    continue;
                }
                match sntp::parse_reply(cookie, &buf[..n]) {
                    Err(sntp::Error::Mismatch) => continue,
                    r => return r.map(|r| sntp::sample(t1, t4, &r)),
                }
            }
        })
        .await;
        match reply {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(sntp::Error::Unsynchronized)) => return Err("server not synchronised"),
            Ok(Err(_)) => err = "malformed reply",
            Err(_) => err = "no reply",
        }
    }
    Err(err)
}
