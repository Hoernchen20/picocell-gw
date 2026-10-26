#!/usr/bin/env python3
"""Firmware update of a gateway over MQTT (sketch.md section 8.1, milestone 8b).

    tools/ota-push.py <gateway-id> [elf] [--host H] [--port P] [--cafile F]
                      [--user U] [--password PW] [--prefix eu868]
                      [--truncate BYTES] [--bad-sha256]

Seals the ELF (default: the fw/ release build) as the cargo runner does (signed with
$PICOCELL_SIGNING_KEY if set), sends the image (the flash contents from 0x10000000 on,
as in the UF2) to command/ota in 4 KB chunks and follows the answers on state/ota. The gateway writes the slot that is not running,
checks the SHA-256 and reboots into the new image; that one has to reach the broker
within 5 minutes, otherwise the gateway falls back to the old image. Defaults: the test
broker (tools/test-broker) with the admin user.

Fault options for tests: --truncate stops after that many bytes (the gateway keeps
running the old image), --bad-sha256 announces a wrong hash (the gateway refuses the
image after the last chunk).

Exits 0 once the gateway reports "verified", 1 on an error answer or timeout.
Standard library only: a minimal MQTT 3.1.1 client over TLS.
"""

import argparse
import hashlib
import os
import socket
import ssl
import struct
import sys
import time
from pathlib import Path

TOOLS = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(TOOLS)
sys.path.insert(0, TOOLS)
sys.dont_write_bytecode = True
elf2uf2 = __import__("elf2uf2-rp2350")

ELF = os.path.join(ROOT, "fw/target/thumbv8m.main-none-eabihf/release/picocell-gw")
CA = os.path.join(TOOLS, "test-broker/out/ca.pem")
CHUNK = 4096
SLOT_SIZE = 988 * 1024
BEGIN, DATA = 1, 2
# Per answer; a reconnecting gateway needs longer, so each chunk is resent a few times.
TIMEOUT = 10
TRIES = 6


class Mqtt:
    """Just enough MQTT 3.1.1: connect, subscribe, QoS 1 publish, receive publishes."""

    def __init__(self, host, port, cafile, user, password):
        ctx = ssl.create_default_context(cafile=cafile)
        # As the other tools: the test broker's certificate names mqtt.test.
        ctx.check_hostname = False
        self.sock = ctx.wrap_socket(socket.create_connection((host, port), timeout=10))
        self.buf = b""
        self.packet_id = 0
        client_id = f"ota-push-{os.getpid()}".encode()
        var = b"\x00\x04MQTT\x04\xc2" + struct.pack(">H", 60)
        payload = self._str(client_id) + self._str(user.encode()) + self._str(password.encode())
        self._send(0x10, var + payload)
        kind, body = self._read(10)
        if kind != 0x20 or len(body) < 2 or body[1] != 0:
            sys.exit(f"broker refused the connection (CONNACK {body.hex()})")

    @staticmethod
    def _str(b):
        return struct.pack(">H", len(b)) + b

    def _send(self, first, body):
        n, length = len(body), b""
        while True:
            byte, n = n % 128, n // 128
            length += bytes([byte | (0x80 if n else 0)])
            if not n:
                break
        self.sock.sendall(bytes([first]) + length + body)

    def _fill(self, n, deadline):
        while len(self.buf) < n:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError
            self.sock.settimeout(left)
            try:
                data = self.sock.recv(65536)
            except (socket.timeout, ssl.SSLWantReadError):
                raise TimeoutError
            if not data:
                sys.exit("broker closed the connection")
            self.buf += data

    def _read(self, timeout):
        """Next packet as (type nibble << 4, body), or TimeoutError."""
        deadline = time.monotonic() + timeout
        self._fill(2, deadline)
        length, shift, i = 0, 0, 1
        while True:
            self._fill(i + 1, deadline)
            byte = self.buf[i]
            length |= (byte & 0x7F) << shift
            shift += 7
            i += 1
            if not byte & 0x80:
                break
        self._fill(i + length, deadline)
        first, body = self.buf[0], self.buf[i : i + length]
        self.buf = self.buf[i + length :]
        return first & 0xF0, body

    def _next_id(self):
        self.packet_id = self.packet_id % 65535 + 1
        return self.packet_id

    def subscribe(self, topic):
        self._send(0x82, struct.pack(">H", self._next_id()) + self._str(topic.encode()) + b"\x00")
        while True:
            kind, body = self._read(10)
            if kind == 0x90:
                if body[2:] == b"\x80":
                    sys.exit(f"subscription to {topic} refused")
                return

    def publish(self, topic, payload):
        """QoS 1, not retained; the PUBACK is skipped when it arrives."""
        self._send(0x32, self._str(topic.encode()) + struct.pack(">H", self._next_id()) + payload)

    def receive(self, timeout):
        """Payload of the next publish (QoS 0, as subscribed), or None on timeout."""
        deadline = time.monotonic() + timeout
        while True:
            try:
                kind, body = self._read(max(0.0, deadline - time.monotonic()))
            except TimeoutError:
                return None
            if kind == 0x30:
                n = struct.unpack(">H", body[:2])[0]
                return body[2 + n :]


def image(elf_path):
    """The sealed image as flat bytes from the start of the slot."""
    out = elf2uf2.flat(elf2uf2.to_pages(elf2uf2.load_segments(elf2uf2.seal(elf_path).read_bytes())))
    if len(out) > SLOT_SIZE:
        sys.exit(f"image ({len(out)} bytes) does not fit a slot ({SLOT_SIZE})")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("gateway_id")
    ap.add_argument("elf", nargs="?", default=ELF)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8883)
    ap.add_argument("--cafile", default=CA)
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default="admin-pw")
    ap.add_argument("--prefix", default="eu868")
    ap.add_argument("--truncate", type=int, metavar="BYTES")
    ap.add_argument("--bad-sha256", action="store_true")
    a = ap.parse_args()

    img = image(Path(a.elf))
    sha = hashlib.sha256(img).digest()
    if a.bad_sha256:
        sha = bytes([sha[0] ^ 1]) + sha[1:]
    base = f"{a.prefix}/gateway/{a.gateway_id}"
    m = Mqtt(a.host, a.port, a.cafile, a.user, a.password)
    m.subscribe(f"{base}/state/ota")
    print(f"{len(img)} bytes, sha256 {hashlib.sha256(img).hexdigest()}")

    def ask(payload, what):
        """Sends until an answer comes; returns it as text."""
        for _ in range(TRIES):
            m.publish(f"{base}/command/ota", payload)
            answer = m.receive(TIMEOUT)
            if answer is not None:
                return answer.decode(errors="replace")
            print(f"no answer to {what}, resending")
        sys.exit(f"no answer from the gateway to {what}")

    started = time.monotonic()
    answer = ask(bytes([BEGIN]) + struct.pack("<I", len(img)) + sha, "begin")
    stop = len(img) if a.truncate is None else min(a.truncate, len(img))
    shown = -1
    while True:
        if answer.startswith("verified"):
            print(f"gateway: {answer} ({time.monotonic() - started:.0f} s)")
            return 0
        if not answer.startswith("next "):
            print(f"gateway: {answer}", file=sys.stderr)
            return 1
        offset = int(answer[5:])
        if offset >= stop:
            print(f"stopped after {offset} bytes (--truncate)")
            return 0
        percent = offset * 100 // len(img)
        if percent // 10 != shown:
            shown = percent // 10
            print(f"{offset:7} / {len(img)} bytes")
        chunk = img[offset : offset + CHUNK]
        answer = ask(bytes([DATA]) + struct.pack("<I", offset) + chunk, f"chunk {offset}")


if __name__ == "__main__":
    sys.exit(main())
