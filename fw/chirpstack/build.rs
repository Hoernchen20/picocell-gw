//! Generates Rust types for the ChirpStack gateway messages with micropb (sketch.md
//! section 9, "Protobuf"). protoc comes from protoc-bin-vendored, no system install.

use std::env;
use std::path::PathBuf;

use micropb_gen::{Config, Generator};

/// Not used over MQTT (Concentratord ZMQ wrappers, mesh, remote exec) or deprecated.
const SKIP: &[&str] = &[
    ".gw.Event",
    ".gw.Command",
    ".gw.MeshEvent",
    ".gw.MeshEventItem",
    ".gw.MeshCommand",
    ".gw.MeshCommandItem",
    ".gw.MeshEventHeartbeat",
    ".gw.MeshEventHeartbeatRelayPath",
    ".gw.MeshEventProprietary",
    ".gw.MeshCommandProprietary",
    ".gw.UplinkFrameSet",
    ".gw.GetGatewayIdRequest",
    ".gw.GetGatewayIdResponse",
    ".gw.GetLocationRequest",
    ".gw.GetLocationResponse",
    ".gw.GatewayCommandExecRequest",
    ".gw.GatewayCommandExecResponse",
    ".gw.RawPacketForwarderEvent",
    ".gw.RawPacketForwarderCommand",
    ".gw.UplinkTxInfoLegacy",
    ".gw.UplinkRxInfoLegacy",
    ".gw.DownlinkTxInfoLegacy",
    ".common.KeyEnvelope",
    ".common.Metric",
    ".common.MetricDataset",
    ".common.JoinServerContext",
    // Deprecated fields; the decoder skips them on the wire.
    ".gw.LoraModulationInfo.code_rate_legacy",
    ".gw.LrFhssModulationInfo.code_rate_legacy",
    ".gw.GatewayStats.gateway_id_legacy",
    ".gw.UplinkFrame.tx_info_legacy",
    ".gw.UplinkFrame.rx_info_legacy",
    ".gw.DownlinkFrame.downlink_id_legacy",
    ".gw.DownlinkFrame.gateway_id_legacy",
    ".gw.DownlinkFrameItem.tx_info_legacy",
    ".gw.DownlinkTxAck.gateway_id_legacy",
    ".gw.DownlinkTxAck.downlink_id_legacy",
    ".gw.GatewayConfiguration.gateway_id_legacy",
    ".gw.ChannelConfiguration.modulation_legacy",
    ".gw.LoraModulationConfig.bandwidth_legacy",
    ".gw.FskModulationConfig.bandwidth_legacy",
    ".gw.ConnState.gateway_id_legacy",
];

fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto = dir.join("proto");
    let include = protoc_bin_vendored::include_path().unwrap();
    // Safety: build scripts are single-threaded.
    unsafe { env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap()) };
    println!("cargo:rerun-if-changed=proto");

    let mut g = Generator::new();
    g.use_container_heapless()
        // One ChirpStack enum panics when the prefix is stripped.
        .retain_enum_prefix(true)
        .add_protoc_arg(format!("-I{}", proto.display()))
        .add_protoc_arg(format!("-I{}", include.display()))
        .configure(".", Config::new().max_bytes(32).max_len(4))
        .configure(".gw.UplinkFrame.phy_payload", Config::new().max_bytes(256))
        .configure(".gw.DownlinkFrameItem.phy_payload", Config::new().max_bytes(256))
        .configure(".gw.UplinkRxInfo.context", Config::new().max_bytes(16))
        .configure(".gw.DownlinkTxInfo.context", Config::new().max_bytes(16))
        .configure(".gw.DownlinkFrame.items", Config::new().max_len(4))
        .configure(".gw.GatewayConfiguration.channels", Config::new().max_len(16))
        .configure(".gw.DutyCycleStats.bands", Config::new().max_len(8))
        // heapless FnvIndexMap needs a power of two.
        .configure_many(
            &[
                ".gw.GatewayStats.tx_packets_per_frequency",
                ".gw.GatewayStats.rx_packets_per_frequency",
                ".gw.GatewayStats.tx_packets_per_status",
            ],
            Config::new().max_len(16),
        )
        .configure_many(
            &[".gw.GatewayStats.tx_packets_per_modulation", ".gw.GatewayStats.rx_packets_per_modulation"],
            Config::new().max_len(8),
        )
        .configure_many(SKIP, Config::new().skip(true));
    // struct.proto is imported by gw.proto but unused, and its recursive Value cannot
    // be generated without an allocator.
    let protos = [
        proto.join("gw/gw.proto"),
        proto.join("common/common.proto"),
        include.join("google/protobuf/timestamp.proto"),
        include.join("google/protobuf/duration.proto"),
    ];
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("chirpstack.rs");
    if let Err(e) = g.compile_protos(&protos, &out) {
        panic!("micropb-gen: {e}");
    }
}
