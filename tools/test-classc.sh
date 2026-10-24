#!/bin/sh
# Class C downlink and duty-cycle test without a network server: sends <count>
# single-item downlinks with "immediately" timing to a gateway on the test broker and
# prints the status of each DownlinkTxAck.
#   tools/test-classc.sh <gateway-id> [count] [frequency-hz] [sf] [power-dbm]
# The defaults (5 × SF12 frames of 1.32 s on 868.85 MHz, a 0.1 % sub-band: 3.6 s per
# hour) give two OK, then DUTY_CYCLE_OVERFLOW; the gateway counts them for an hour.
# 869525000 (10 %, 27 dBm) is the band ChirpStack uses for Class C.
set -eu

ID=${1:?usage: test-classc.sh <gateway-id> [count] [frequency-hz] [sf] [power-dbm]}
COUNT=${2:-5}
FREQ=${3:-868850000}
SF=${4:-12}
POWER=${5:-14}
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

# One file per ack: mosquitto_sub -N writes the binary payloads back to back.
mqtt mosquitto_sub -t "$T/event/ack" -C "$COUNT" -W 20 -F '%x' > "$TMP/acks" &
ACKS=$!
sleep 1

i=0
while [ "$i" -lt "$COUNT" ]; do
    i=$((i + 1))
    "$TOOLS/pb-decode.sh" -e DownlinkFrame > "$TMP/down" <<PB
downlink_id: $i
gateway_id: "$ID"
items {
  phy_payload: "\240picocell class c $i"
  tx_info {
    frequency: $FREQ power: $POWER
    modulation { lora { bandwidth: 125000 spreading_factor: $SF code_rate: CR_4_5 polarization_inversion: true } }
    timing { immediately {} }
  }
}
PB
    mqtt mosquitto_pub -t "$T/command/down" -q 1 -s < "$TMP/down"
done
echo "$COUNT downlinks sent ($FREQ Hz, SF$SF, $POWER dBm, immediately)"

wait $ACKS || { echo "fewer than $COUNT DownlinkTxAcks within 20 s" >&2; exit 1; }
while read -r hex; do
    printf '%s' "$hex" | xxd -r -p | "$TOOLS/pb-decode.sh" DownlinkTxAck > "$TMP/ack"
    id=$(sed -n 's/^downlink_id: //p' "$TMP/ack")
    status=$(sed -n 's/^ *status: //p' "$TMP/ack")
    echo "downlink ${id:-0}: ${status:-IGNORED}"
done < "$TMP/acks"
