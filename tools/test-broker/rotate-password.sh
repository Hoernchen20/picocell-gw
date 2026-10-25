#!/bin/sh
# Changes a gateway's broker password remotely on the test broker (sketch.md 5.2).
#
#   rotate-password.sh <gateway-id> <password-file>
#
# The file holds the new password: 1 to 64 printable ASCII characters, no blanks (a
# trailing newline is ignored). Steps:
#   1. The password goes to command/credentials, raw from stdin (no shell quoting, no
#      newline) and not retained. The gateway stores it next to its current one and
#      answers "stored" on state/credentials.
#   2. The broker's password file gets the new password and the broker reloads it,
#      which disconnects the gateway. It reconnects with the new password, stores it
#      for good, answers "committed" and restarts.
# Without "committed" the old broker password is restored; the gateway then connects
# with its stored password again and drops the new one.
#
# The broker must be running. PREFIX overrides the topic prefix (default eu868).
set -eu

DIR=$(cd "$(dirname "$0")" && pwd)/out
USAGE="usage: rotate-password.sh <gateway-id> <password-file>"
ID=${1:?$USAGE}
FILE=${2:?$USAGE}
PREFIX=${PREFIX:-eu868}
export LC_ALL=C

PW=$(cat "$FILE")
case $PW in
    "" | *[!!-~]*) echo "$FILE: 1 to 64 printable characters expected, no blanks" >&2; exit 1 ;;
esac
if [ ${#PW} -gt 64 ]; then
    echo "$FILE: 1 to 64 printable characters expected, no blanks" >&2
    exit 1
fi

mqtt() {
    tool=$1
    shift
    "$tool" -h 127.0.0.1 -p 8883 --cafile "$DIR/ca.pem" --insecure -u admin -P admin-pw -V mqttv5 "$@"
}

ANSWERS=$(mktemp)
SUB=
cleanup() {
    [ -z "$SUB" ] || kill "$SUB" 2>/dev/null || true
    rm -f "$ANSWERS" "$DIR/passwd.old"
}
trap cleanup EXIT

# Waits up to $1 seconds for the next answer: sets A to it, or empty on timeout.
# Not called in $(...): the count of answers read (N) must survive.
answer() {
    i=0
    while [ "$i" -lt "$1" ]; do
        A=$(sed -n "$((N + 1))p" "$ANSWERS")
        if [ -n "$A" ]; then
            N=$((N + 1))
            return
        fi
        sleep 1
        i=$((i + 1))
    done
}

mqtt mosquitto_sub -t "$PREFIX/gateway/$ID/state/credentials" > "$ANSWERS" &
SUB=$!
N=0
sleep 1

printf '%s' "$PW" | mqtt mosquitto_pub -t "$PREFIX/gateway/$ID/command/credentials" -q 1 -s
answer 10
case $A in
    stored) echo "gateway: stored" ;;
    "") echo "no answer from the gateway (connected?); nothing changed" >&2; exit 1 ;;
    *) echo "gateway: $A" >&2; exit 1 ;;
esac

# On the command line, visible to local users: acceptable for the test broker.
cp -p "$DIR/passwd" "$DIR/passwd.old"
mosquitto_passwd -b "$DIR/passwd" "$ID" "$PW"
pkill -HUP -x mosquitto
echo "broker: switched to the new password"

answer 90
if [ "$A" = committed ]; then
    echo "gateway: committed, restarting with the new password"
    echo "note: provisioning with the old password file would set the old password again"
    exit 0
fi
echo "gateway: ${A:-no answer}" >&2
cp -p "$DIR/passwd.old" "$DIR/passwd"
pkill -HUP -x mosquitto
echo "broker: old password restored; the gateway falls back to its stored one" >&2
exit 1
