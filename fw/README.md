# picocell-gw firmware

Rust/Embassy firmware for the W5500-EVB-Pico2 (RP2350). Design: [../sketch.md](../sketch.md).

Current state: Ethernet (DHCP or static IPv4), hosts table + DNS, SNTP, TLS 1.3 +
MQTT 5 to the broker with ChirpStack protobuf messages: retained `ConnState` with Last
Will, `GatewayStats` every 30 s (RX counters zero, duty-cycle load per sub-band), a
test `UplinkFrame` every 10 s (proprietary frame, made-up radio metadata), and
`DownlinkFrame`s from `command/down` (Class A delay and Class C immediately timing)
checked against the EU868 duty cycle, scheduled in the JIT queue and answered with a
`DownlinkTxAck` on `event/ack`; log, commands and provisioning on the USB console,
config encrypted in flash. No radio yet: a downlink is logged on the console at its
start time instead of sent, and the RP2350 timer stands in for the SX1302 counter.

## Requirements

- Rust with the target: `rustup target add thumbv8m.main-none-eabihf`
- Python 3 (UF2 conversion), Mosquitto + OpenSSL for the test broker

## Test broker

```sh
tools/test-broker/setup.sh            # once; creates tools/test-broker/out/
mosquitto -c tools/test-broker/out/mosquitto.conf
```

The broker listens on port 8883 on all interfaces; allow it in the firewall. The
gateway looks up `mqtt.host` (`mqtt.test`) in its hosts table (`net.hosts`) and checks
the certificate against that name, so no DNS entry is needed.

## Build and flash

```sh
cd fw
cargo run --release
```

`cargo run` builds, writes `target/thumbv8m.main-none-eabihf/release/picocell-gw.uf2`
and copies it to the board if it is mounted in BOOTSEL mode (hold BOOTSEL while
plugging in USB). Otherwise copy the `.uf2` to the `RP2350` drive by hand.

## Configuration and provisioning

The config lives in the last 64 KB of flash (behind the image, so flashing keeps it),
encrypted with AES-128-GCM. For now the key is derived from the chip ID, which only
keeps the secrets out of a plain flash dump; step 8 moves to a random key in OTP, and
boards then need provisioning again. Without a complete config the gateway stays
offline in provisioning mode and says so on the console.

| Key | Default | |
|---|---|---|
| `net.mode` | `static` | `static` or `dhcp` |
| `net.ipv4` | | address/prefix, e.g. `10.0.0.20/24`; required with `static` |
| `net.gateway` | | default gateway |
| `net.dns` | | up to 3 DNS servers, comma-separated; tried before the ones from DHCP |
| `net.hosts` | | up to 4 `name=address` entries, checked before DNS |
| `ntp.servers` | `0.pool.ntp.org,1.pool.ntp.org,2.pool.ntp.org` | up to 3, IP or host name |
| `mqtt.host` | | broker host name (SNI and certificate check); required |
| `mqtt.port` | `8883` | |
| `mqtt.topic_prefix` | `eu868` | |
| `mqtt.password` | | required; 1 to 64 printable ASCII characters, no blanks; the username is the gateway ID |
| `tls.ca0`, `tls.ca1` | | broker CA (PEM via `ca 0` / `ca 1`); `tls.ca0` required |

With `tls.ca1` set, a failed TLS handshake makes the next attempt use the other slot
(CA rotation). [../tools/provision.py](../tools/provision.py) sends a config file over
the console and commits it; for the test broker:

```sh
tools/provision.py tools/test-broker/gateway.conf net.hosts=mqtt.test=<IP of the broker host>
```

[gateway.conf](../tools/test-broker/gateway.conf) uses DHCP and reads the password and
CA from `tools/test-broker/out/`. On the console the same works by hand: `show`, `set
<key> <value>`, `ca 0` and paste the PEM file, `commit`. A value starting with `@` in
a config file names a file; write `@@` for a literal `@`.

### Remote password change

The new password, raw and not retained, on `<prefix>/gateway/<id>/command/credentials`
(only admin/ChirpStack may publish there) is stored next to the current one; the
gateway answers `stored` or `rejected: <reason>` on `state/credentials`. The next
connection uses the new password: accepted, it replaces the stored one (`committed`,
then a restart); refused, the gateway falls back to the stored one and drops the new
one when that is accepted (`rejected: broker refused it, ...`). A restart in between
keeps the staged password; `status` shows it. Change the broker only after `stored`:
Mosquitto disconnects the gateway when its password changes on reload, which starts
the next connection. For the test broker:

```sh
tools/test-broker/rotate-password.sh <gateway-id> <file with the new password>
```

It sends the password from the file (no shell quoting involved), waits for `stored`,
switches the broker's password file and waits for `committed`; otherwise it restores
the old broker password. ChirpStack may log the unknown `state/credentials` messages.

## Time

The broker connection waits for the first SNTP result (`ntp.servers`), because
certificate dates cannot be checked without it. The pool names need DNS: from DHCP,
or `net.dns` with a static address. Without internet access, point `ntp.servers` at a
local server. With three servers the median is used, so one wrong server is outvoted;
a time before the build time is rejected.

## Console and first connection

The USB-C port is a serial console (CDC-ACM) for log output and commands, e.g.
`picocom /dev/ttyACM0` (or `/dev/serial/by-id/usb-picocell_picocell-gw_console_<gateway
id>-if00`). The last 8 KB of log lines are kept in RAM and replayed when a terminal
opens the port, so the boot messages show up even after connecting late; after that
new lines appear live above the input line. Commands (`help` lists them):

| Command | |
|---|---|
| `status` | firmware, gateway ID, MAC, IP, time, uptime, MQTT state, duty-cycle load |
| `log` / `log off\|warn\|info` | show / set the live log level (`info` after boot, not saved) |
| `log show [lines]` | the whole history, or its last lines, regardless of the level |
| `show` | the config being edited; secrets as `***`, certificates as size and SHA-256 |
| `set <key> <value>` | change a key in the edited config; `set mqtt.password` alone asks without echo |
| `unset <key>` | back to the default (or none), also `tls.ca0` / `tls.ca1` |
| `ca 0\|1` | paste a PEM certificate into `tls.ca0` / `tls.ca1` |
| `clear` / `discard` | start from an empty config / drop the changes since the last commit |
| `commit` | store the edited config and restart with it |
| `erase` | delete the stored config and restart in provisioning mode |
| `reboot` | restart |
| `bootsel` | restart into the USB bootloader, so `cargo run` flashes without the button |

Each command ends with `ok` or `error: ...` on its own line. Changes stay in RAM until
`commit`; `status` notes uncommitted ones. MAC and gateway ID are
derived from the RP2350 chip ID, so several boards can share a network; the boot
message and `status` show them. Register each board with the broker using that ID:

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

[../tools/test-classc.sh](../tools/test-classc.sh) sends Class C downlinks
("immediately" timing, no uplink needed) and prints one status per frame. The
defaults fill the duty cycle of the 0.1 % sub-band 868.7–869.2 MHz (3.6 s per hour)
with SF12 frames of 1.32 s:

```sh
tools/test-classc.sh <gateway-id>                      # 2 × OK, 3 × DUTY_CYCLE_OVERFLOW
tools/test-classc.sh <gateway-id> 3 869525000 12 27    # RX2 band, 10 %: 3 × OK
tools/test-classc.sh <gateway-id> 1 868850000 12 20    # TX_POWER (16 dBm there)
tools/test-classc.sh <gateway-id> 1 868650000          # TX_FREQ (between sub-bands)
```

The frames go out back to back: each starts 3.5 ms after the previous one ends
(start delay and margins), as the queue keeps a frame's airtime blocked until it is
sent. The airtime counts against
its sub-band for an hour (`duty_cycle_stats` in `event/stats`); a reboot clears it.

## Hardware-in-the-loop test

[../tools/hil-test.py](../tools/hil-test.py) tests a board on USB and Ethernet against
the running test broker, over the USB console only: console commands, provisioning
with `gateway.conf` (`net.hosts` pointing at this machine), MQTT connection,
`ConnState`, `GatewayStats`, `UplinkFrame`, and the downlink and Class C cases above
with their expected statuses. It prints PASS/FAIL per check and exits with the number
of failures:

```sh
tools/hil-test.py                  # provisions, then tests (about 2 minutes)
tools/hil-test.py --flash          # builds and flashes first (via "bootsel")
tools/hil-test.py --no-provision   # keeps the stored config, reboots instead
tools/hil-test.py --erase          # deletes the config at the end
```

Close other terminals on the console first: two programs reading the port each get
part of the output (the test refuses to start then). `--broker-ip` overrides the
address of this machine, by default the one on the default route. Without a debug
probe a hung firmware needs BOOTSEL by hand.

## Host tests

[wire](wire) holds the DNS and SNTP packet code without I/O:
`cd wire && cargo test --target x86_64-unknown-linux-gnu`.

[chirpstack](chirpstack) generates the ChirpStack gateway messages with `micropb`
from the protos in [chirpstack/proto](chirpstack/proto) (ChirpStack v4.20.0) and
encodes the published ones; its tests decode the output with `protoc`:
`cd chirpstack && cargo test --target x86_64-unknown-linux-gnu`.

[console](console) holds the log history ring and the line editor of the USB console:
`cd console && cargo test --target x86_64-unknown-linux-gnu`.

[settings](settings) holds the config keys, their checks, the stored form and the PEM
input: `cd settings && cargo test --target x86_64-unknown-linux-gnu`.

[jit](jit) is the downlink queue (too late/too early, collisions, first free slot,
counter wrap), the LoRa airtime and the EU868 duty cycle:
`cd jit && cargo test --target x86_64-unknown-linux-gnu`.

## Regression test for the MQTT/TLS stack

[../tools/mqtt-host-test](../tools/mqtt-host-test) runs the same `rust-mqtt` +
`embedded-tls` stack on the PC against the test broker. Run it after bumping either
crate: `cd tools/mqtt-host-test && cargo run --release`, and once more with `CHOP=3`.
