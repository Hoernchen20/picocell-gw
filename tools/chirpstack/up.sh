#!/bin/sh
# Starts ChirpStack and the test broker (docker-compose.yml). Needs tools/test-broker/out/
# from setup.sh. Other docker compose commands (logs, down, ...) need the same
# variables: HOST_UID=$(id -u) HOST_GID=$(id -g) docker compose ...
set -eu

DIR=$(cd "$(dirname "$0")" && pwd)
[ -f "$DIR/../test-broker/out/passwd" ] || { echo "no test broker: run tools/test-broker/setup.sh" >&2; exit 1; }
if pgrep -x -u "$(id -u)" mosquitto >/dev/null && ! docker compose -f "$DIR/docker-compose.yml" ps -q mosquitto 2>/dev/null | grep -q .; then
    echo "a local mosquitto is running (port 8883); stop it first" >&2
    exit 1
fi
HOST_UID=$(id -u) HOST_GID=$(id -g) exec docker compose -f "$DIR/docker-compose.yml" up -d "$@"
