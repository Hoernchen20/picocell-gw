#!/bin/sh
# Adds a gateway (username = gateway ID from the board's USB console) to the test
# broker, with the password from out/gateway-password (provisioned via gateway.conf).
# A running broker reloads the password file on SIGHUP.
#
#   add-gateway.sh <gateway-id>
set -eu

DIR=$(cd "$(dirname "$0")" && pwd)/out
ID=${1:?usage: add-gateway.sh <gateway-id>}
mosquitto_passwd -b "$DIR/passwd" "$ID" "$(cat "$DIR/gateway-password")"
pkill -HUP -x mosquitto && echo "added $ID, broker reloaded" || echo "added $ID (broker not running)"
