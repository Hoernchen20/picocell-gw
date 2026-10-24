//! ChirpStack gateway messages (sketch.md section 3.3), generated with micropb, and
//! encoders for the ones the gateway publishes.

#![no_std]

#[allow(clippy::all, nonstandard_style, unused, irrefutable_let_patterns)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/chirpstack.rs"));
}

pub use generated::common_ as common;
pub use generated::google_::protobuf_ as protobuf;
pub use generated::gw_ as gw;

use heapless::{String, Vec};
use micropb::{MessageEncode, PbEncoder};

/// `buf` is too small or a string or byte field exceeds its capacity.
#[derive(Debug, PartialEq)]
pub struct Overflow;

/// Encodes `msg` into `buf` and returns the encoded length.
pub fn encode<M: MessageEncode>(msg: &M, buf: &mut [u8]) -> Result<usize, Overflow> {
    let total = buf.len();
    let mut rest = &mut buf[..];
    msg.encode(&mut PbEncoder::new(&mut rest)).map_err(|_| Overflow)?;
    Ok(total - rest.len())
}

fn string<const N: usize>(s: &str) -> Result<String<N>, Overflow> {
    String::try_from(s).map_err(|_| Overflow)
}

fn timestamp(unix_us: i64) -> protobuf::Timestamp {
    protobuf::Timestamp {
        seconds: unix_us.div_euclid(1_000_000),
        nanos: (unix_us.rem_euclid(1_000_000) * 1000) as i32,
    }
}

pub fn conn_state(gateway_id: &str, online: bool, buf: &mut [u8]) -> Result<usize, Overflow> {
    let state = if online { gw::ConnState_::State::Online } else { gw::ConnState_::State::Offline };
    encode(&gw::ConnState { gateway_id: string(gateway_id)?, state }, buf)
}

/// Radio metadata of one received LoRa frame.
pub struct LoraRx {
    pub uplink_id: u32,
    pub frequency: u32,
    pub bandwidth: u32,
    pub spreading_factor: u32,
    pub code_rate: gw::CodeRate,
    pub rssi: i32,
    pub snr: f32,
    pub channel: u32,
    pub rf_chain: u32,
    /// SX1302 1 MHz counter at the end of the frame; echoed in Class A downlinks.
    pub count_us: u32,
    pub crc_ok: bool,
    /// Wall clock time of reception, if known.
    pub time_us: Option<i64>,
}

pub fn uplink_frame(
    gateway_id: &str,
    phy_payload: &[u8],
    rx: &LoraRx,
    buf: &mut [u8],
) -> Result<usize, Overflow> {
    let modulation = gw::Modulation {
        parameters: Some(gw::Modulation_::Parameters::Lora(gw::LoraModulationInfo {
            bandwidth: rx.bandwidth,
            spreading_factor: rx.spreading_factor,
            code_rate: rx.code_rate,
            ..Default::default()
        })),
    };
    let mut tx_info = gw::UplinkTxInfo::default();
    tx_info.set_frequency(rx.frequency).set_modulation(modulation);

    let mut rx_info = gw::UplinkRxInfo::default();
    rx_info
        .set_gateway_id(string(gateway_id)?)
        .set_uplink_id(rx.uplink_id)
        .set_rssi(rx.rssi)
        .set_snr(rx.snr)
        .set_channel(rx.channel)
        .set_rf_chain(rx.rf_chain)
        // Big-endian, as ChirpStack Concentratord does.
        .set_context(Vec::from_slice(&rx.count_us.to_be_bytes()).unwrap())
        .set_crc_status(if rx.crc_ok { gw::CRCStatus::CrcOk } else { gw::CRCStatus::BadCrc });
    if let Some(t) = rx.time_us {
        rx_info.set_gw_time(timestamp(t));
    }

    let mut frame = gw::UplinkFrame::default();
    frame
        .set_phy_payload(Vec::from_slice(phy_payload).map_err(|_| Overflow)?)
        .set_tx_info(tx_info)
        .set_rx_info(rx_info);
    encode(&frame, buf)
}

/// Packet counters since the previous stats message.
#[derive(Default)]
pub struct Counters {
    pub rx_received: u32,
    pub rx_received_ok: u32,
    pub tx_received: u32,
    pub tx_emitted: u32,
}

pub fn gateway_stats(
    gateway_id: &str,
    time_us: i64,
    counters: &Counters,
    metadata: &[(&str, &str)],
    buf: &mut [u8],
) -> Result<usize, Overflow> {
    let mut stats = gw::GatewayStats::default();
    stats.set_gateway_id(string(gateway_id)?).set_time(timestamp(time_us));
    stats.rx_packets_received = counters.rx_received;
    stats.rx_packets_received_ok = counters.rx_received_ok;
    stats.tx_packets_received = counters.tx_received;
    stats.tx_packets_emitted = counters.tx_emitted;
    for (k, v) in metadata {
        stats.metadata.insert(string(k)?, string(v)?).map_err(|_| Overflow)?;
    }
    encode(&stats, buf)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::string::String;

    /// Decodes with the reference implementation: `protoc --decode`.
    fn protoc_decode(msg_type: &str, bytes: &[u8]) -> String {
        let proto = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
        let include = protoc_bin_vendored::include_path().unwrap();
        let mut child = Command::new(protoc_bin_vendored::protoc_bin_path().unwrap())
            .arg(std::format!("--decode={msg_type}"))
            .arg(std::format!("-I{proto}"))
            .arg(std::format!("-I{}", include.display()))
            .arg("gw/gw.proto")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// Normalises protoc's text format to one `key: value` / `key {` / `}` per line.
    fn lines(s: &str) -> std::vec::Vec<&str> {
        s.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
    }

    const ID: &str = "0016c001ff10a235";

    #[test]
    fn conn_state_interop() {
        let mut buf = [0u8; 64];
        let n = conn_state(ID, true, &mut buf).unwrap();
        assert_eq!(lines(&protoc_decode("gw.ConnState", &buf[..n])), [
            "state: ONLINE",
            "gateway_id: \"0016c001ff10a235\""
        ]);
        // OFFLINE is the proto3 default and therefore not on the wire.
        let n = conn_state(ID, false, &mut buf).unwrap();
        assert_eq!(lines(&protoc_decode("gw.ConnState", &buf[..n])), [
            "gateway_id: \"0016c001ff10a235\""
        ]);
    }

    #[test]
    fn uplink_frame_interop() {
        let rx = LoraRx {
            uplink_id: 1234,
            frequency: 868_100_000,
            bandwidth: 125_000,
            spreading_factor: 7,
            code_rate: gw::CodeRate::Cr45,
            rssi: -57,
            snr: 9.5,
            channel: 0,
            rf_chain: 1,
            count_us: 0x0102_0304,
            crc_ok: true,
            time_us: Some(1_792_849_845_250_000),
        };
        let mut buf = [0u8; 512];
        let n = uplink_frame(ID, &[0xe0, 1, 2], &rx, &mut buf).unwrap();
        assert_eq!(lines(&protoc_decode("gw.UplinkFrame", &buf[..n])), [
            "phy_payload: \"\\340\\001\\002\"",
            "tx_info {",
            "frequency: 868100000",
            "modulation {",
            "lora {",
            "bandwidth: 125000",
            "spreading_factor: 7",
            "code_rate: CR_4_5",
            "}",
            "}",
            "}",
            "rx_info {",
            "gateway_id: \"0016c001ff10a235\"",
            "uplink_id: 1234",
            "gw_time {",
            "seconds: 1792849845",
            "nanos: 250000000",
            "}",
            "rssi: -57",
            "snr: 9.5",
            "rf_chain: 1",
            "context: \"\\001\\002\\003\\004\"",
            "crc_status: CRC_OK",
            "}",
        ]);
    }

    #[test]
    fn gateway_stats_interop() {
        let c = Counters { rx_received: 3, rx_received_ok: 2, tx_received: 1, tx_emitted: 1 };
        let mut buf = [0u8; 512];
        let n = gateway_stats(ID, 1_792_849_845_000_000, &c, &[("firmware", "0.1.0")], &mut buf)
            .unwrap();
        assert_eq!(lines(&protoc_decode("gw.GatewayStats", &buf[..n])), [
            "time {",
            "seconds: 1792849845",
            "}",
            "rx_packets_received: 3",
            "rx_packets_received_ok: 2",
            "tx_packets_received: 1",
            "tx_packets_emitted: 1",
            "metadata {",
            "key: \"firmware\"",
            "value: \"0.1.0\"",
            "}",
            "gateway_id: \"0016c001ff10a235\"",
        ]);
    }

    #[test]
    fn overflow() {
        let mut buf = [0u8; 8];
        assert_eq!(conn_state(ID, true, &mut buf), Err(Overflow));
        let mut buf = [0u8; 64];
        assert_eq!(conn_state(&"x".repeat(33), true, &mut buf), Err(Overflow));
        let c = Counters::default();
        let meta = [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4"), ("e", "5")];
        assert_eq!(gateway_stats(ID, 0, &c, &meta, &mut [0u8; 512]), Err(Overflow));
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(1_500_000), protobuf::Timestamp { seconds: 1, nanos: 500_000_000 });
        assert_eq!(timestamp(-1), protobuf::Timestamp { seconds: -1, nanos: 999_999_000 });
    }
}
