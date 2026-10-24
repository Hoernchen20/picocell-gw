#!/bin/sh
# Decodes one ChirpStack gateway message from stdin with protoc, e.g.
#   mosquitto_sub ... -t 'eu868/gateway/+/event/stats' -C 1 -N | tools/pb-decode.sh GatewayStats
# With -e, encodes protoc text format from stdin instead.
# Uses protoc from PATH, else the copy vendored by fw/chirpstack's build
# (protoc-bin-vendored in the cargo registry).
set -eu

MODE=decode
if [ "${1:-}" = -e ]; then
    MODE=encode
    shift
fi
TYPE=${1:?usage: pb-decode.sh [-e] <ConnState|UplinkFrame|GatewayStats|DownlinkFrame|...>}
DIR=$(cd "$(dirname "$0")/.." && pwd)/fw/chirpstack/proto

if command -v protoc >/dev/null; then
    PROTOC=protoc
    INCLUDE=
else
    VENDORED=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/protoc-bin-vendored-linux-x86_64-* | tail -n 1)
    PROTOC=$VENDORED/bin/protoc
    INCLUDE=-I$VENDORED/include
fi
# gw.proto imports struct.proto without using it; drop protoc's warning about that.
ERR=$(mktemp)
trap 'rm -f "$ERR"' EXIT
STATUS=0
"$PROTOC" --$MODE="gw.$TYPE" -I"$DIR" $INCLUDE gw/gw.proto 2>"$ERR" || STATUS=$?
grep -v 'struct.proto is unused' "$ERR" >&2 || true
exit $STATUS
