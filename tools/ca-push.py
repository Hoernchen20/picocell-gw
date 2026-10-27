#!/usr/bin/env python3
"""Signed CA update of a gateway over MQTT (sketch.md section 8.1, milestone 8e).

    tools/ca-push.py <gateway-id> [cert] [--clear] [--key PEM] [--seq N] [--retain]
                     [--save FILE] [--message FILE] [--host H] [--port P]
                     [--cafile F] [--user U] [--password PW] [--prefix eu868]

Signs the CA certificate (PEM or DER) with the config-signing key (default the test
key fw/keys/test-config-signing.pem, which dev builds trust) and sends it to
command/ca; the gateway stores it in tls.ca1 and restarts. --clear sends an empty
certificate instead, which clears tls.ca1. Message: sequence number (u64 LE),
certificate (DER), ECDSA P-256 signature r || s over SHA-256("picocell-gw ca
update\\0" || sequence number || certificate). The sequence number defaults to the
current Unix time and has to be above the one the gateway stored last.

The message is not bound to a gateway: --save writes it to a file and --message
sends a saved one (to another gateway, or again). --retain leaves it on the broker
for gateways that are offline; remove it with mosquitto_pub -r -n -t <topic>.

Exits 0 when the gateway answers "stored" or "current" on state/ca, 1 otherwise.
Needs openssl for the signature.
"""

import argparse
import hashlib
import os
import ssl
import struct
import subprocess
import sys
import tempfile
import time

TOOLS = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(TOOLS)
sys.path.insert(0, TOOLS)
sys.dont_write_bytecode = True
Mqtt = __import__("ota-push").Mqtt

KEY = os.path.join(ROOT, "fw/keys/test-config-signing.pem")
CA = os.path.join(TOOLS, "test-broker/out/ca.pem")
DOMAIN = b"picocell-gw ca update\0"
# settings::CA_MAX
CA_MAX = 1024
TIMEOUT = 10
TRIES = 3


def certificate(path):
    data = open(path, "rb").read()
    if b"-----BEGIN CERTIFICATE-----" in data:
        data = ssl.PEM_cert_to_DER_cert(data.decode())
    if not data or data[0] != 0x30:
        sys.exit(f"{path}: not a certificate")
    if len(data) > CA_MAX:
        sys.exit(f"{path}: {len(data)} bytes, at most {CA_MAX} fit")
    return data


def raw_signature(der):
    """ECDSA-Sig-Value (DER) to r || s, 32 bytes each."""
    def integer(i):
        if der[i] != 2:
            sys.exit("openssl: unexpected signature format")
        n = der[i + 1]
        return int.from_bytes(der[i + 2 : i + 2 + n], "big"), i + 2 + n

    if der[0] != 0x30 or der[1] >= 0x80:
        sys.exit("openssl: unexpected signature format")
    r, i = integer(2)
    s, _ = integer(i)
    return r.to_bytes(32, "big") + s.to_bytes(32, "big")


def sign(key, signed):
    with tempfile.NamedTemporaryFile() as f:
        f.write(DOMAIN + signed)
        f.flush()
        r = subprocess.run(["openssl", "dgst", "-sha256", "-sign", key, f.name], capture_output=True)
    if r.returncode != 0:
        sys.exit(f"openssl: {r.stderr.decode().strip()}")
    return raw_signature(r.stdout)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("gateway_id")
    ap.add_argument("cert", nargs="?", help="CA certificate, PEM or DER")
    ap.add_argument("--clear", action="store_true", help="clear tls.ca1 instead")
    ap.add_argument("--key", default=KEY, help="config-signing key (PEM)")
    ap.add_argument("--seq", type=int, help="sequence number (default: Unix time)")
    ap.add_argument("--retain", action="store_true")
    ap.add_argument("--save", metavar="FILE", help="also write the signed message to FILE")
    ap.add_argument("--message", metavar="FILE", help="send this signed message")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8883)
    ap.add_argument("--cafile", default=CA)
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default="admin-pw")
    ap.add_argument("--prefix", default="eu868")
    a = ap.parse_args()

    if a.message:
        if a.cert or a.clear:
            ap.error("--message replaces cert and --clear")
        msg = open(a.message, "rb").read()
    else:
        if bool(a.cert) == a.clear:
            ap.error("a certificate or --clear expected")
        der = b"" if a.clear else certificate(a.cert)
        seq = int(time.time()) if a.seq is None else a.seq
        signed = struct.pack("<Q", seq) + der
        msg = signed + sign(a.key, signed)
        if a.save:
            open(a.save, "wb").write(msg)
    seq = struct.unpack("<Q", msg[:8])[0]
    what = f"{len(msg) - 72} bytes, sha256 {hashlib.sha256(msg[8:-64]).hexdigest()}" if len(msg) > 72 else "clear"
    print(f"sequence number {seq}, certificate: {what}")

    base = f"{a.prefix}/gateway/{a.gateway_id}"
    m = Mqtt(a.host, a.port, a.cafile, a.user, a.password)
    m.subscribe(f"{base}/state/ca")
    for _ in range(TRIES):
        m.publish(f"{base}/command/ca", msg, a.retain)
        answer = m.receive(TIMEOUT)
        if answer is not None:
            answer = answer.decode(errors="replace")
            print(f"gateway: {answer}")
            return 0 if answer.startswith(("stored", "current")) else 1
        print("no answer, resending")
    print("no answer from the gateway (connected?)", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
