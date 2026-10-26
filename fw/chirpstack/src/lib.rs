//! ChirpStack gateway messages (sketch.md section 3.3), generated with micropb, with
//! encoders for the ones the gateway publishes and a decoder for downlinks.

#![no_std]

#[allow(clippy::all, nonstandard_style, unused, irrefutable_let_patterns)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/chirpstack.rs"));
}

pub use generated::common_ as common;
pub use generated::google_::protobuf_ as protobuf;
pub use generated::gw_ as gw;

use heapless::{String, Vec};
use micropb::{MessageDecode, MessageEncode, PbEncoder};

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

/// Not a valid message, or a string or byte field exceeds its capacity.
#[derive(Debug, PartialEq)]
pub struct Malformed;

pub fn decode<M: MessageDecode + Default>(bytes: &[u8]) -> Result<M, Malformed> {
    let mut msg = M::default();
    msg.decode_from_bytes(bytes).map_err(|_| Malformed)?;
    Ok(msg)
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

fn duration(us: u64) -> protobuf::Duration {
    protobuf::Duration { seconds: (us / 1_000_000) as i64, nanos: (us % 1_000_000 * 1000) as i32 }
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

/// Duty-cycle load of one sub-band.
pub struct DutyBand<'a> {
    pub name: &'a str,
    pub frequency_min: u32,
    pub frequency_max: u32,
    pub load_max_us: u64,
    pub load_tracked_us: u64,
}

/// ETSI EN 300 220 duty-cycle tracking.
pub struct DutyCycle<'a> {
    pub window_us: u64,
    pub bands: &'a [DutyBand<'a>],
}

pub fn gateway_stats(
    gateway_id: &str,
    time_us: i64,
    counters: &Counters,
    duty_cycle: Option<&DutyCycle>,
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
    if let Some(d) = duty_cycle {
        let mut dc = gw::DutyCycleStats { regulation: common::Regulation::EtsiEn300220, ..Default::default() };
        dc.set_window(duration(d.window_us));
        for b in d.bands {
            let mut band = gw::DutyCycleBand {
                name: string(b.name)?,
                frequency_min: b.frequency_min,
                frequency_max: b.frequency_max,
                ..Default::default()
            };
            band.set_load_max(duration(b.load_max_us)).set_load_tracked(duration(b.load_tracked_us));
            dc.bands.push(band).map_err(|_| Overflow)?;
        }
        stats.set_duty_cycle_stats(dc);
    }
    encode(&stats, buf)
}

/// When a downlink item is to be sent.
#[derive(Debug, PartialEq)]
pub enum Timing {
    Immediately,
    /// `delay_us` after the SX1302 counter value from the uplink context.
    Delay { count_us: u32, delay_us: u32 },
    GpsEpoch,
}

/// A downlink item the gateway cannot send, for logs.
#[derive(Debug, PartialEq)]
pub enum Unsupported {
    /// FSK, LR-FHSS or none.
    Modulation,
    CodeRate,
    /// No timing given.
    Timing,
    /// Not 4 bytes, so not from one of our uplinks.
    Context,
    /// Negative or longer than `u32` µs.
    Delay,
}

/// The parameters of one LoRa downlink item.
#[derive(Debug, PartialEq)]
pub struct LoraTx<'a> {
    pub frequency: u32,
    pub power: i32,
    pub bandwidth: u32,
    pub spreading_factor: u32,
    /// 1–4 for 4/5–4/8.
    pub code_rate: u32,
    pub invert_polarity: bool,
    pub preamble: u32,
    pub crc: bool,
    pub timing: Timing,
    pub payload: &'a [u8],
}

pub fn lora_tx(item: &gw::DownlinkFrameItem) -> Result<LoraTx<'_>, Unsupported> {
    let tx = item.tx_info().ok_or(Unsupported::Modulation)?;
    let Some(gw::Modulation_::Parameters::Lora(lora)) =
        tx.modulation().and_then(|m| m.parameters.as_ref())
    else {
        return Err(Unsupported::Modulation);
    };
    let code_rate = match lora.code_rate {
        gw::CodeRate::Cr45 => 1,
        gw::CodeRate::Cr46 => 2,
        gw::CodeRate::Cr47 => 3,
        gw::CodeRate::Cr48 => 4,
        _ => return Err(Unsupported::CodeRate),
    };
    let timing = match tx.timing().and_then(|t| t.parameters.as_ref()) {
        None => return Err(Unsupported::Timing),
        Some(gw::Timing_::Parameters::Immediately(_)) => Timing::Immediately,
        Some(gw::Timing_::Parameters::GpsEpoch(_)) => Timing::GpsEpoch,
        Some(gw::Timing_::Parameters::Delay(d)) => {
            let context: [u8; 4] = tx.context.as_slice().try_into().map_err(|_| Unsupported::Context)?;
            let d = d.delay().copied().unwrap_or_default();
            let us = d.seconds.checked_mul(1_000_000).and_then(|s| s.checked_add(i64::from(d.nanos / 1000)));
            Timing::Delay {
                count_us: u32::from_be_bytes(context),
                delay_us: us.and_then(|us| u32::try_from(us).ok()).ok_or(Unsupported::Delay)?,
            }
        }
    };
    Ok(LoraTx {
        frequency: tx.frequency,
        power: tx.power,
        bandwidth: lora.bandwidth,
        spreading_factor: lora.spreading_factor,
        code_rate,
        invert_polarity: lora.polarization_inversion,
        // ChirpStack leaves it unset; LoRaWAN uses 8 symbols.
        preamble: if lora.preamble == 0 { 8 } else { lora.preamble },
        crc: !lora.no_crc,
        timing,
        payload: &item.phy_payload,
    })
}

/// `statuses` has one entry per item of the `DownlinkFrame`, in the same order.
pub fn tx_ack(
    gateway_id: &str,
    downlink_id: u32,
    statuses: &[gw::TxAckStatus],
    buf: &mut [u8],
) -> Result<usize, Overflow> {
    let mut ack = gw::DownlinkTxAck { gateway_id: string(gateway_id)?, downlink_id, ..Default::default() };
    for status in statuses {
        ack.items.push(gw::DownlinkTxAckItem { status: *status }).map_err(|_| Overflow)?;
    }
    encode(&ack, buf)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::string::String;

    /// Runs `protoc --decode` or `--encode` (the reference implementation) on `input`.
    fn protoc(mode: &str, msg_type: &str, input: &[u8]) -> std::vec::Vec<u8> {
        let proto = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
        let include = protoc_bin_vendored::include_path().unwrap();
        let mut child = Command::new(protoc_bin_vendored::protoc_bin_path().unwrap())
            .arg(std::format!("--{mode}={msg_type}"))
            .arg(std::format!("-I{proto}"))
            .arg(std::format!("-I{}", include.display()))
            .arg("gw/gw.proto")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    fn protoc_decode(msg_type: &str, bytes: &[u8]) -> String {
        String::from_utf8(protoc("decode", msg_type, bytes)).unwrap()
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
        let bands = [DutyBand {
            name: "869.4-869.65",
            frequency_min: 869_400_000,
            frequency_max: 869_650_000,
            load_max_us: 360_000_000,
            load_tracked_us: 1_155_072,
        }];
        let duty = DutyCycle { window_us: 3_600_000_000, bands: &bands };
        let n = gateway_stats(ID, 1_792_849_845_000_000, &c, Some(&duty), &[("firmware", "0.1.0")], &mut buf)
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
            "duty_cycle_stats {",
            "regulation: ETSI_EN_300_220",
            "window {",
            "seconds: 3600",
            "}",
            "bands {",
            "name: \"869.4-869.65\"",
            "frequency_min: 869400000",
            "frequency_max: 869650000",
            "load_max {",
            "seconds: 360",
            "}",
            "load_tracked {",
            "seconds: 1",
            "nanos: 155072000",
            "}",
            "}",
            "}",
        ]);
    }

    /// A Class A downlink as ChirpStack sends it: RX1 and RX2, delay timing.
    const DOWNLINK: &str = r#"
        downlink_id: 4242
        gateway_id: "0016c001ff10a235"
        items {
          phy_payload: "\140\001\002"
          tx_info {
            frequency: 868100000
            power: 14
            modulation { lora { bandwidth: 125000 spreading_factor: 7 code_rate: CR_4_5 polarization_inversion: true } }
            timing { delay { delay { seconds: 1 } } }
            context: "\001\002\003\004"
          }
        }
        items {
          phy_payload: "\140\001\002"
          tx_info {
            frequency: 869525000
            power: 27
            modulation { lora { bandwidth: 125000 spreading_factor: 12 code_rate: CR_4_5 polarization_inversion: true no_crc: true preamble: 10 } }
            timing { delay { delay { seconds: 2 nanos: 500000 } } }
            context: "\001\002\003\004"
          }
        }
    "#;

    #[test]
    fn downlink_frame_interop() {
        let bytes = protoc("encode", "gw.DownlinkFrame", DOWNLINK.as_bytes());
        let frame: gw::DownlinkFrame = decode(&bytes).unwrap();
        assert_eq!(frame.downlink_id, 4242);
        assert_eq!(frame.gateway_id.as_str(), ID);
        assert_eq!(frame.items.len(), 2);
        assert_eq!(lora_tx(&frame.items[0]), Ok(LoraTx {
            frequency: 868_100_000,
            power: 14,
            bandwidth: 125_000,
            spreading_factor: 7,
            code_rate: 1,
            invert_polarity: true,
            preamble: 8,
            crc: true,
            timing: Timing::Delay { count_us: 0x0102_0304, delay_us: 1_000_000 },
            payload: &[0x60, 1, 2],
        }));
        let rx2 = lora_tx(&frame.items[1]).unwrap();
        assert_eq!((rx2.spreading_factor, rx2.preamble, rx2.crc), (12, 10, false));
        assert_eq!(rx2.timing, Timing::Delay { count_us: 0x0102_0304, delay_us: 2_000_500 });
    }

    #[test]
    fn downlink_unsupported() {
        let item = |tx_info: &str| {
            let text = std::format!("items {{ tx_info {{ {tx_info} }} }}");
            let frame: gw::DownlinkFrame = decode(&protoc("encode", "gw.DownlinkFrame", text.as_bytes())).unwrap();
            frame.items[0].clone()
        };
        let lora = "modulation { lora { bandwidth: 125000 spreading_factor: 7 code_rate: CR_4_5 } }";
        assert_eq!(lora_tx(&item("modulation { fsk { datarate: 50000 } }")).err(), Some(Unsupported::Modulation));
        assert_eq!(lora_tx(&item(lora)).err(), Some(Unsupported::Timing));
        let t = std::format!("{lora} timing {{ immediately {{}} }}");
        assert_eq!(lora_tx(&item(&t)).unwrap().timing, Timing::Immediately);
        let t = std::format!("{lora} timing {{ gps_epoch {{}} }}");
        assert_eq!(lora_tx(&item(&t)).unwrap().timing, Timing::GpsEpoch);
        let t = std::format!("{lora} timing {{ delay {{ delay {{ seconds: 1 }} }} }} context: \"\\001\"");
        assert_eq!(lora_tx(&item(&t)).err(), Some(Unsupported::Context));
        let t = std::format!("{lora} timing {{ delay {{ delay {{ seconds: -1 }} }} }} context: \"abcd\"");
        assert_eq!(lora_tx(&item(&t)).err(), Some(Unsupported::Delay));
        let t = "modulation { lora { code_rate: CR_LI_4_6 } } timing { immediately {} }";
        assert_eq!(lora_tx(&item(t)).err(), Some(Unsupported::CodeRate));
        assert_eq!(decode::<gw::DownlinkFrame>(&[0xff, 0xff]).err(), Some(Malformed));
    }

    #[test]
    fn tx_ack_interop() {
        let mut buf = [0u8; 128];
        let statuses = [gw::TxAckStatus::TooLate, gw::TxAckStatus::Ok];
        let n = tx_ack(ID, 4242, &statuses, &mut buf).unwrap();
        assert_eq!(lines(&protoc_decode("gw.DownlinkTxAck", &buf[..n])), [
            "downlink_id: 4242",
            "items {",
            "status: TOO_LATE",
            "}",
            "items {",
            "status: OK",
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
        let meta: [_; 9] = core::array::from_fn(|i| (["a", "b", "c", "d", "e", "f", "g", "h", "i"][i], "1"));
        assert_eq!(gateway_stats(ID, 0, &c, None, &meta, &mut [0u8; 512]), Err(Overflow));
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(1_500_000), protobuf::Timestamp { seconds: 1, nanos: 500_000_000 });
        assert_eq!(timestamp(-1), protobuf::Timestamp { seconds: -1, nanos: 999_999_000 });
    }
}
