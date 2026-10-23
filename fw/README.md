# picocell-gw firmware

Rust/Embassy firmware for the W5500-EVB-Pico2 (RP2350). Design: [../sketch.md](../sketch.md).

Current state: Ethernet (DHCP or static IPv4), TLS 1.3 + MQTT 5 to the broker,
retained `state/conn` with Last Will, `command/down` subscription and a test uplink
every 10 s. No radio, SNTP, config store or provisioning yet; settings are build-time
values in [.cargo/config.toml](.cargo/config.toml).

## Requirements

- Rust with the target: `rustup target add thumbv8m.main-none-eabihf`
- Python 3 (UF2 conversion), Mosquitto + OpenSSL for the test broker

## Test broker

```sh
tools/test-broker/setup.sh            # once; creates tools/test-broker/out/
mosquitto -c tools/test-broker/out/mosquitto.conf
```

The broker listens on port 8883 on all interfaces; allow it in the firewall. The
firmware reaches it by IP (`PICOCELL_MQTT_ADDR`) and checks the certificate against
`PICOCELL_MQTT_HOST` (`mqtt.test`), so no DNS entry is needed.

## Build and flash

```sh
cd fw
PICOCELL_MQTT_ADDR=<IP of the broker host> cargo run --release
```

`cargo run` builds, writes `target/thumbv8m.main-none-eabihf/release/picocell-gw.uf2`
and copies it to the board if it is mounted in BOOTSEL mode (hold BOOTSEL while
plugging in USB). Otherwise copy the `.uf2` to the `RP2350` drive by hand.

Other settings (see `.cargo/config.toml`) are overridden the same way, e.g.
`PICOCELL_NET=10.0.0.20/24 PICOCELL_NET_GATEWAY=10.0.0.1`.

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

## Regression test for the MQTT/TLS stack

[../tools/mqtt-host-test](../tools/mqtt-host-test) runs the same `rust-mqtt` +
`embedded-tls` stack on the PC against the test broker. Run it after bumping either
crate: `cd tools/mqtt-host-test && cargo run --release`, and once more with `CHOP=3`.
