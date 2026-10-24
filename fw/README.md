# picocell-gw firmware

Rust/Embassy firmware for the W5500-EVB-Pico2 (RP2350). Design: [../sketch.md](../sketch.md).

Current state: Ethernet (DHCP or static IPv4), hosts table + DNS, SNTP, TLS 1.3 +
MQTT 5 to the broker with ChirpStack protobuf messages: retained `ConnState` with Last
Will, `GatewayStats` every 30 s (RX counters zero), a test `UplinkFrame` every 10 s
(proprietary frame, made-up radio metadata), and `DownlinkFrame`s from `command/down`
scheduled in the JIT queue and answered with a `DownlinkTxAck` on `event/ack`. No
radio yet: a downlink is logged on the console at its start time instead of sent, and
the RP2350 timer stands in for the SX1302 counter. No config store or provisioning
yet; settings are build-time values in
[.cargo/config.toml](.cargo/config.toml).

## Requirements

- Rust with the target: `rustup target add thumbv8m.main-none-eabihf`
- Python 3 (UF2 conversion), Mosquitto + OpenSSL for the test broker

## Test broker

```sh
tools/test-broker/setup.sh            # once; creates tools/test-broker/out/
mosquitto -c tools/test-broker/out/mosquitto.conf
```

The broker listens on port 8883 on all interfaces; allow it in the firewall. The
firmware looks up `PICOCELL_MQTT_HOST` (`mqtt.test`) in the hosts table
(`PICOCELL_HOSTS`) and checks the certificate against that name, so no DNS entry is
needed.

## Build and flash

```sh
cd fw
PICOCELL_HOSTS=mqtt.test=<IP of the broker host> cargo run --release
```

`cargo run` builds, writes `target/thumbv8m.main-none-eabihf/release/picocell-gw.uf2`
and copies it to the board if it is mounted in BOOTSEL mode (hold BOOTSEL while
plugging in USB). Otherwise copy the `.uf2` to the `RP2350` drive by hand.

Other settings (see `.cargo/config.toml`) are overridden the same way, e.g.
`PICOCELL_NET=10.0.0.20/24 PICOCELL_NET_GATEWAY=10.0.0.1`.

## Time

The broker connection waits for the first SNTP result (`PICOCELL_NTP`, default
`0.pool.ntp.org` … `2.pool.ntp.org`), because certificate dates cannot be checked
without it. The pool names need DNS: from DHCP, or `PICOCELL_DNS` with a static
address. Without internet access, point `PICOCELL_NTP` at a local server, e.g.
`PICOCELL_NTP=10.0.0.1`. With three servers the median is used, so one wrong server
is outvoted; a time before the build time is rejected.

## Console and first connection

The USB-C port is a serial console (CDC-ACM), e.g. `picocom /dev/ttyACM0`. Every 30 s
the board prints its gateway ID, MAC and IP. MAC and gateway ID are derived from the
RP2350 chip ID, so several boards can share a network. Register each board with the
broker using that ID:

```sh
tools/test-broker/add-gateway.sh <gateway-id>
```

Watch its traffic:

```sh
mosquitto_sub -h 127.0.0.1 -p 8883 --cafile tools/test-broker/out/ca.pem --insecure \
    -u admin -P admin-pw -V mqttv5 -t 'eu868/gateway/#' -v
```

The payloads are protobuf; decode one message with
[../tools/pb-decode.sh](../tools/pb-decode.sh) (no system `protoc` needed):

```sh
mosquitto_sub -h 127.0.0.1 -p 8883 --cafile tools/test-broker/out/ca.pem --insecure \
    -u admin -P admin-pw -V mqttv5 -t 'eu868/gateway/+/event/stats' -C 1 -N \
    | tools/pb-decode.sh GatewayStats
```

Likewise `event/up` with `UplinkFrame` and `state/conn` with `ConnState` (add `-R` to
skip the retained message and wait for a new one).

## Downlink test

Without a network server, [../tools/test-downlink.sh](../tools/test-downlink.sh)
answers the next test uplink like a Class A downlink from ChirpStack (RX1 after 1 s
on 868.1 MHz SF7, RX2 one second later on 869.525 MHz SF12, delay timing with the
uplink's context) and prints the `DownlinkTxAck`:

```sh
tools/test-downlink.sh <gateway-id>       # RX1 OK, RX2 IGNORED
tools/test-downlink.sh <gateway-id> 0     # RX1 TOO_LATE, RX2 OK
tools/test-downlink.sh <gateway-id> 61    # both TOO_EARLY (limit 60 s from queuing)
```

The console shows the queued frame and, at its start time, `TX (no radio)`. An
`IGNORED` item decodes as an empty `items {}` (proto3 default value).

## Host tests

[wire](wire) holds the DNS and SNTP packet code without I/O:
`cd wire && cargo test --target x86_64-unknown-linux-gnu`.

[chirpstack](chirpstack) generates the ChirpStack gateway messages with `micropb`
from the protos in [chirpstack/proto](chirpstack/proto) (ChirpStack v4.20.0) and
encodes the published ones; its tests decode the output with `protoc`:
`cd chirpstack && cargo test --target x86_64-unknown-linux-gnu`.

[jit](jit) is the downlink queue (too late/too early, collisions, first free slot,
counter wrap) and the LoRa airtime:
`cd jit && cargo test --target x86_64-unknown-linux-gnu`.

## Regression test for the MQTT/TLS stack

[../tools/mqtt-host-test](../tools/mqtt-host-test) runs the same `rust-mqtt` +
`embedded-tls` stack on the PC against the test broker. Run it after bumping either
crate: `cd tools/mqtt-host-test && cargo run --release`, and once more with `CHOP=3`.
