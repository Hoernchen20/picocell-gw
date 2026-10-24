#!/bin/sh
# Class A downlink test without a network server: waits for the next uplink of a
# gateway on the test broker, answers it like ChirpStack would (RX1 after the given
# delay, RX2 one second later, delay timing with the uplink's context) and prints the
# DownlinkTxAck.
#   tools/test-downlink.sh <gateway-id> [rx1-delay-s]    default 1
# A delay of 0 makes RX1 too late (RX2 is sent), 61 makes both too early: the 60 s
# limit counts from when the gateway queues the frame, a little after the uplink.
set -eu

ID=${1:?usage: test-downlink.sh <gateway-id> [rx1-delay-s]}
RX1=${2:-1}
RX2=$((RX1 + 1))
TOOLS=$(cd "$(dirname "$0")" && pwd)
T=eu868/gateway/$ID
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mqtt() {
    cmd=$1
    shift
    "$cmd" -h 127.0.0.1 -p 8883 --cafile "$TOOLS/test-broker/out/ca.pem" --insecure \
        -u admin -P admin-pw -V mqttv5 "$@"
}

# Subscribed before the uplink arrives, so nothing delays the downlink.
mqtt mosquitto_sub -t "$T/event/ack" -C 1 -N -W 40 > "$TMP/ack" &
ACK=$!
echo "waiting for an uplink from $ID ..."
mqtt mosquitto_sub -t "$T/event/up" -C 1 -N -W 30 > "$TMP/up"
CONTEXT=$("$TOOLS/pb-decode.sh" UplinkFrame < "$TMP/up" | grep -o 'context: ".*"')
DOWNLINK_ID=$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')

"$TOOLS/pb-decode.sh" -e DownlinkFrame > "$TMP/down" <<PB
downlink_id: $DOWNLINK_ID
gateway_id: "$ID"
items {
  phy_payload: "\340picocell down"
  tx_info {
    frequency: 868100000 power: 14
    modulation { lora { bandwidth: 125000 spreading_factor: 7 code_rate: CR_4_5 polarization_inversion: true } }
    timing { delay { delay { seconds: $RX1 } } }
    $CONTEXT
  }
}
items {
  phy_payload: "\340picocell down"
  tx_info {
    frequency: 869525000 power: 27
    modulation { lora { bandwidth: 125000 spreading_factor: 12 code_rate: CR_4_5 polarization_inversion: true } }
    timing { delay { delay { seconds: $RX2 } } }
    $CONTEXT
  }
}
PB
mqtt mosquitto_pub -t "$T/command/down" -q 1 -s < "$TMP/down"
echo "downlink $DOWNLINK_ID sent (uplink $CONTEXT, RX1 +${RX1} s, RX2 +${RX2} s)"

wait $ACK || { echo "no DownlinkTxAck within 40 s" >&2; exit 1; }
"$TOOLS/pb-decode.sh" DownlinkTxAck < "$TMP/ack"
