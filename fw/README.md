# picocell-gw firmware

Rust/Embassy firmware for the W5500-EVB-Pico2 (RP2350). Design: [../sketch.md](../sketch.md).

Current state: Ethernet (DHCP or static IPv4), hosts table + DNS, SNTP, TLS 1.3 +
MQTT 5 to the broker with ChirpStack protobuf messages: retained `ConnState` with Last
Will, `GatewayStats` every 30 s (packet counters, duty-cycle load per sub-band),
`UplinkFrame`s from the WM1302, which runs Semtech's sx1302_hal on core 1 (below), and
`DownlinkFrame`s from `command/down` (retained ones ignored; Class A delay and Class C
immediately timing) checked against the EU868 duty cycle, scheduled in the JIT queue
on the SX1302 counter and answered with a `DownlinkTxAck` on `event/ack`; log,
commands and provisioning on the USB console, config encrypted in flash. A downlink
is logged at its start time instead of sent. While the radio is not running (no
module), dev builds publish a test `UplinkFrame` every 10 s instead (proprietary
frame, made-up radio metadata) and the RP2350 timer stands in for the SX1302 counter.

## Requirements

- Rust with the target: `rustup target add thumbv8m.main-none-eabihf`
- Arm GNU toolchain with newlib (sx1302_hal is C): `arm-none-eabi-gcc` on the PATH,
  e.g. `apt install gcc-arm-none-eabi libnewlib-arm-none-eabi`
- Python 3 (UF2 conversion), Mosquitto + OpenSSL for the test broker
- [picotool](https://github.com/raspberrypi/picotool) 2.x (sealing, partition table):
  prebuilt binaries in the `pico-sdk-tools` releases; the runner finds it on the
  PATH, in `~/.local/bin` or via `$PICOTOOL`

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

`cargo run` builds, seals the image (`picotool seal --hash`, version from `Cargo.toml`),
writes `target/thumbv8m.main-none-eabihf/release/picocell-gw.uf2` and copies it to the
board if it is mounted in BOOTSEL mode (hold BOOTSEL while plugging in USB). Otherwise copy the `.uf2` to the `RP2350` drive by hand.

### Partition table

The firmware runs with or without the A/B partition table
([partition-table.json](partition-table.json), sketch.md section 3.1: 8 KB table,
slots A and B of 988 KB each, 64 KB config at `0x1F0000`, readable only by secure
code). It reads and writes the config through the boot ROM's `flash_op`, which works
in both cases. To install the table, with the board in BOOTSEL (`bootsel` on the
console):

```sh
picotool partition create partition-table.json target/pt.uf2
picotool load target/pt.uf2
picotool reboot -u
picotool partition info
```

Copying `pt.uf2` to the `RP2350` drive does not work: the ROM ignores it. picotool
needs USB access to the board in BOOTSEL, e.g. a udev rule for vendor `2e8a`. Then
flash the firmware again (the table overwrites the start of an image flashed without
it); the config partition keeps its contents.

With the table, a UF2 copy goes to the slot that is not running, so copies alternate
between A and B; `status` shows the slot and boot type (`boot` line). The image is
flagged TBYB (try before you buy): after a UF2 copy the ROM boots it as a trial and
falls back to the other slot unless the firmware buys it. After a UF2 copy it does so
right at boot (`explicit_buy` in the log); after an OTA update only once it reaches
the broker (below). A bought image is the only one left: the ROM erases the first
sector of the other slot.

The firmware runs the watchdog (8 s, fed every second while the executor runs); it
also takes over the ROM's 16.7 s trial watchdog, which DHCP alone can nearly use up.

### Firmware update over MQTT

With the table installed and the gateway connected, from the repository root:

```sh
cargo build --release --manifest-path fw/Cargo.toml
tools/ota-push.py <gateway-id>
```

`ota-push.py` seals the ELF as the runner does and sends the image to
`command/ota` in 4 KB chunks (QoS 1, admin user of the test broker; `--host`,
`--user` etc. for another broker). The gateway writes the slot that is not running,
checks the SHA-256, answers on `state/ota` and reboots into the new image (about 8 s
for 330 KB). The new image runs as a trial (`trial` line in `status`): it is bought
once it has connected to the broker and subscribed; otherwise it reboots after 5
minutes and the old image boots again, as on any reboot before the buy. An update
is refused while the running image is still on trial (the other slot is its
fallback). A transfer survives MQTT reconnects (unanswered chunks are resent) but
not a reboot of the gateway; then run `ota-push.py` again.

Fault options for tests: `--bad-sha256` (refused after the last chunk), `--truncate
BYTES` (the gateway keeps running the old image).

### Signing

Without a key the image is only hashed: dev boards (no secure boot in OTP) boot it as
it is. With `PICOCELL_SIGNING_KEY` set to a secp256k1 PEM key, the runner and
`ota-push.py` also sign it (`picotool seal --sign`) and check the result with
`tools/verify-image.py`, which recomputes hash and signature in plain Python against
the key:

```sh
PICOCELL_SIGNING_KEY=$PWD/keys/test-signing.pem cargo run --release
../tools/verify-image.py target/thumbv8m.main-none-eabihf/release/picocell-gw.uf2
```

[keys/test-signing.pem](keys/test-signing.pem) is a test key, committed on purpose:
only for boards whose OTP holds its hash (a lock board, sketch.md section 8.1), never
for real gateways. The production key is separate and stays offline. `verify-image.py`
prints the boot key hash that goes into OTP `BOOTKEY0`.

`cargo run --release --features production` builds a production image: the build
needs `PICOCELL_CONFIG_PUBKEY` (the public config-signing key, see "CA rotation"
below), the runner refuses it without a signing key and adds the rollback version from `[package.metadata.picocell]`
in [Cargo.toml](Cargo.toml) (dev images have none, so buying one never touches the OTP
rollback rows). `status` and the `GatewayStats` metadata show the build (`dev`/`prod`)
and, read from OTP, secure boot, debug disable and the valid boot keys.
A production build stays offline (console only, no MQTT) until the board is locked:
secure boot on, a boot key that is not revoked, debug disabled in OTP. Provisioning
works before that; `status` names the missing step.

`tools/test-signing.py [--production ELF]` tests the tooling on the host: picotool
and `verify-image.py` agree, tampered code or signature and another key fail, the
cleared TBYB flag (after `explicit_buy`) still verifies, a production build without a
key is refused.

## Configuration and provisioning

The config lives in the last 64 KB of flash (behind both image slots, so flashing
keeps it), encrypted with AES-128-GCM. Dev builds derive the key from the chip ID, which only
keeps the secrets out of a plain flash dump. Production builds derive it from 16
random bytes in OTP page 20, which `otp-key init` writes once (only with secure boot
on, after typing `yes`; it cannot be undone) and locks for good: secure read-only, no
non-secure or bootloader access. There is no migration: a board switching to a
production build is provisioned again. Both builds read the page at boot and then
soft-lock it until the next reset; `otp-key` and the `config key` line in `status`
show the key source, the page state and the lock. Without a key (production, page
empty or damaged) or without a complete config the gateway stays offline in
provisioning mode and says so on the console.

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
| `console.password` | | console login, below; required in production builds |
| `tls.ca0`, `tls.ca1` | | broker CA (PEM via `ca 0` / `ca 1`); `tls.ca0` required |

With `tls.ca1` set, a failed TLS handshake makes the next attempt use the other slot
(CA rotation, below). [../tools/provision.py](../tools/provision.py) sends a config file over
the console and commits it; for the test broker:

```sh
tools/provision.py tools/test-broker/gateway.conf net.hosts=mqtt.test=<IP of the broker host>
```

[gateway.conf](../tools/test-broker/gateway.conf) uses DHCP and reads the password and
CA from `tools/test-broker/out/`. On the console the same works by hand: `show`, `set
<key> <value>`, `ca 0` and paste the PEM file, `commit`. A value starting with `@` in
a config file names a file; write `@@` for a literal `@`. `provision.py` hashes
`console.password` before sending it, and logs in first if the console is locked
(password from `$PICOCELL_CONSOLE_PASSWORD`, else it asks).

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

### CA rotation

A new broker CA reaches the gateways before the broker switches to it, signed with
the config-signing key (ECDSA P-256, separate from the boot key): on
`<prefix>/gateway/<id>/command/ca` it goes into `tls.ca1`, and the gateway answers
`stored <sequence number>, restarting` on `state/ca`. Every update carries a sequence
number (by default the Unix time of signing) that has to be above the last one the
gateway stored (`ca update` in `status`): the same one again is answered `current`, an
older one `rejected`, so a recorded update cannot be replayed later. It survives
`commit`, only `erase` resets it. The message is not bound to a gateway, so one
signature serves all of them, and it may be retained for gateways that are offline.
An empty certificate clears `tls.ca1`.

Once the broker presents a certificate from the new CA, the handshake with `tls.ca0`
fails, the next one uses `tls.ca1`, and when that session is up the gateway makes
`tls.ca1` its `tls.ca0`, drops the old CA (`promoted: ...` on `state/ca`) and
restarts.

```sh
tools/ca-push.py <gateway-id> new-ca.pem               # signs with the test key
tools/ca-push.py <gateway-id> new-ca.pem --save update.bin
tools/ca-push.py <other-gateway-id> --message update.bin
tools/ca-push.py <gateway-id> --clear                  # clears tls.ca1
```

Dev builds trust [keys/test-config-signing.pem](keys/test-config-signing.pem) (a test
key, committed on purpose). Other builds take the public key from
`PICOCELL_CONFIG_PUBKEY` (PEM), which production builds require; `ca-push.py --key`
signs with the matching private key. An image can also bring a CA:
`PICOCELL_CA=<PEM or DER>` at build time puts it into `tls.ca1` at boot, once, when it is
in neither slot and the build is newer than the last CA update (so an old image does
not bring back a replaced CA).

On the test broker, `tools/test-broker/rotate-ca.sh issue` creates a second CA and
broker certificate in `out/next/`, and `rotate-ca.sh switch` swaps them with the ones
in `out/` (again to switch back). Mosquitto keeps open connections on reload, so the
gateway sees the new certificate on its next connection (`reboot`).

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
| `login` / `logout` | unlock / lock the console, with `console.password` set |

Each command ends with `ok` or `error: ...` on its own line. Changes stay in RAM until
`commit`; `status` notes uncommitted ones.

With `console.password` stored, the console opens locked: only `help`, `status`, `log`,
`login`, `logout` and `erase` work until `login` and the password (no echo). It locks
again on `logout`, when the port closes, and after 10 minutes without input. A wrong
password delays the answer, 3 s doubling to 60 s, until a correct one or a restart.
`set console.password` asks for the new password twice and stores it as
PBKDF2-HMAC-SHA256 (10000 iterations, random salt, `iterations$salt$hash`; such a
hash can also be given as the value). Forgotten password: `erase` works without login
and deletes the whole config, MQTT password included, so the board has to be
provisioned again.
 MAC and gateway ID are
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
answers the next uplink (a test uplink, or a real one while the radio runs) like a
Class A downlink from ChirpStack (RX1 after 1 s
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

## Radio

[sx1302](sx1302) builds Semtech's sx1302_hal V2.1.0 (in `sx1302/hal`, unmodified,
BSD license) with `cc`; `sx1302/shim` replaces its Linux SPI, I2C, clock and printf
code. The HAL blocks for seconds while it starts and polls the SX1302 over SPI, so it
runs on core 1; flash writes park core 1 in RAM meanwhile.

At boot core 1 switches the WM1302 on (GP15), resets the SX1302 (GP14, active high)
and probes it over SPI1 at 2 MHz (GP10 SCK, GP11 MOSI, GP12 MISO, GP13 CS): version
register, and a write and read-back of the page register, which needs all four SPI
lines. Then `lgw_start` loads the AGC/ARB firmware, calibrates both SX1250s and
switches the SX1302 to their 32 MHz clock, with the EU868 channel plan of Semtech's
`global_conf.json.sx1250.EU868` (867.1–868.5 MHz multi-SF, 868.3 MHz SF7/250 kHz, FSK
868.8 MHz). `lgw_start` also needs the module's STTS751 temperature sensor on I2C0
(GP4 SDA, GP5 SCL, pull-ups external) and fails without it. The log shows

```
radio: SX1302 version 0x10 (v1.0), page register ok, starting the concentrator
sx1302_hal: ...
radio: running after ... ms, EU868, eui ...
radio: rx 868.100 MHz SF7 BW125 4/5, crc ok, rssi -57 dBm, snr 9.5 dB, 23 bytes, count_us ...
```

Frames with a valid CRC go to ChirpStack as `UplinkFrame` on `event/up` (LoRa only;
an FSK frame is logged and dropped), with the SX1302 `count_us` as `context` and the
time core 1 fetched the frame as `gw_time`; `GatewayStats` counts all frames and those
with a valid CRC. After each poll core 1 also reads the SX1302 counter, and the
downlink queue extrapolates from that read with the RP2350 timer, so Class A delays
count on the clock the uplink's `count_us` came from.

`radio` on the console shows the state, probe result, EUI and frame counters;
`radio restart` power-cycles the module and starts again, handy while rewiring. A
version of `0x00` or `0xff` means MISO floats or the module has no power;
`page register failed` with a plausible version points at MOSI or CS;
`no temperature sensor found` from sx1302_hal at I2C. The EUI is only valid once
`lgw_start` ran (before, the OTP reads back a constant).

Desk test with the Seeed WM1302 Pi HAT (Pi header pin numbers): GP10 → 23, GP11 → 19,
GP12 ← 21, GP13 → 24, GP14 → 11, GP15 → 12 through 1 kΩ, GP4 ↔ 3 (SDA), GP5 → 5
(SCL), 4.7 kΩ from SDA and SCL to 3V3 (Pico pin 36), VBUS (Pico pin 40) → 2, GND → 6
and 25. Antenna on before anything transmits.

## Hardware-in-the-loop test

[../tools/hil-test.py](../tools/hil-test.py) tests a board on USB and Ethernet against
the running test broker, over the USB console only: console commands, provisioning
with `gateway.conf` (`net.hosts` pointing at this machine), MQTT connection,
`ConnState`, `GatewayStats`, `UplinkFrame`, and the downlink and Class C cases above
with their expected statuses (with the WM1302 running, `UplinkFrame` and the Class A
cases need a device that sends at least every 15 s). It prints PASS/FAIL per check and
exits with the number of failures:

```sh
tools/hil-test.py                  # provisions, then tests (about 2 minutes)
tools/hil-test.py --flash          # builds and flashes first (via "bootsel")
tools/hil-test.py --no-provision   # keeps the stored config, reboots instead
tools/hil-test.py --erase          # deletes the config at the end
tools/hil-test.py --ota            # OTA: refused images, update, kept after reboot
tools/hil-test.py --ota-fallback   # trials that never reach the broker (about 7 minutes)
tools/hil-test.py --ca             # CA rotation there and back (about 2 minutes)
```

The OTA checks need the partition table. `--ota-fallback` changes the gateway's
password on the test broker during the trial and restores it afterwards (also on
failure). `--ca` rejects an update signed with the wrong key, a replayed and an older
one, then rotates to the CA in `out/next/` and back, so the broker ends as it began
(after a failure it switches the broker back; re-provision the gateway then).

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
