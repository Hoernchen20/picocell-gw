#!/usr/bin/env python3
"""Host test of the signing tooling (sketch.md section 8.1, 8c), no board or OTP needed.

    tools/test-signing.py [elf] [--production ELF]

Seals the ELF (default: the fw/ release build) as the runner does with the test key
and checks tools/verify-image.py against picotool's own check: the signed image passes
and its boot key hash matches the OTP value picotool computes; a changed code byte, a
changed signature and another key fail; a cleared TBYB flag (as after explicit_buy)
still passes; an unsigned image passes only with --unsigned. With --production (an ELF
built with `--features production`): refused without a key, signed with a rollback
version. Exits with the number of failed checks.
"""

import argparse
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

TOOLS = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(TOOLS)
sys.path.insert(0, TOOLS)
sys.dont_write_bytecode = True
elf2uf2 = __import__("elf2uf2-rp2350")
verify = __import__("verify-image")

ELF = os.path.join(ROOT, "fw/target/thumbv8m.main-none-eabihf/release/picocell-gw")
KEY = os.path.join(ROOT, "fw/keys/test-signing.pem")

failed = 0


def expect(name, ok):
    global failed
    print(f"{'ok  ' if ok else 'FAIL'} {name}")
    failed += not ok


def outcome(img, key, unsigned=False):
    try:
        verify.check(img, key, unsigned)
        return "ok"
    except verify.Invalid as e:
        return str(e)


def sealed(elf, key):
    if key:
        os.environ["PICOCELL_SIGNING_KEY"] = key
    else:
        os.environ.pop("PICOCELL_SIGNING_KEY", None)
    return elf2uf2.flat(elf2uf2.to_pages(elf2uf2.load_segments(elf2uf2.seal(elf).read_bytes())))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("elf", nargs="?", default=ELF)
    ap.add_argument("--production", help="ELF built with --features production")
    a = ap.parse_args()
    tmp = Path(tempfile.mkdtemp(prefix="test-signing-"))
    try:
        elf = tmp / "fw.elf"
        shutil.copy(a.elf, elf)
        if elf2uf2.has_symbol(elf.read_bytes(), b"PICOCELL_PRODUCTION"):
            sys.exit(f"{a.elf}: needs a dev build")
        prod_elf = None
        if a.production:
            prod_elf = tmp / "prod.elf"
            shutil.copy(a.production, prod_elf)
            if not elf2uf2.has_symbol(prod_elf.read_bytes(), b"PICOCELL_PRODUCTION"):
                sys.exit(f"{a.production}: not a production build")
        key = verify.public_key(KEY)
        img = sealed(elf, KEY)
        expect("signed image verifies", outcome(img, key) == "ok")

        otp = tmp / "otp.json"
        subprocess.run([elf2uf2.picotool(), "seal", "--quiet", "--sign", str(elf), "-t", "elf",
                        str(tmp / "otp.elf"), "-t", "elf", KEY, str(otp)], check=True)
        boot_key = bytes(json.loads(otp.read_text())["bootkey0"]).hex()
        expect("boot key hash matches picotool's OTP value",
               verify.check(img, key)["boot_key_hash"] == boot_key)

        start = verify.check(img, key)["block"] - verify.FLASH_START
        code = bytearray(img)
        code[0x1000] ^= 1
        expect("changed code byte fails", "hash mismatch" in outcome(bytes(code), key))
        sig = bytearray(img)
        _, its = [b for b in verify.blocks(img) if b[0] == start][0]
        o = next(o for t, o, _ in its if t == verify.SIGNATURE)
        sig[o + 100] ^= 1
        expect("changed signature fails", outcome(bytes(sig), key) == "bad signature")
        bought = bytearray(img)
        assert struct.unpack_from("<I", img, start + 4)[0] & 0x80000000
        bought[start + 7] &= 0x7F
        expect("cleared TBYB flag still verifies", outcome(bytes(bought), key) == "ok")
        other = tmp / "other.pem"
        subprocess.run(["openssl", "ecparam", "-name", "secp256k1", "-genkey", "-noout", "-out", str(other)],
                       check=True)
        expect("another key fails", outcome(img, verify.public_key(other)) == "signed with another key")
        expect("picotool agrees on the signed image",
               "signature:           verified" in subprocess.run(
                   [elf2uf2.picotool(), "info", "-a", str(elf) + ".sealed.elf", "-t", "elf"],
                   capture_output=True, text=True).stdout)

        plain = sealed(elf, None)
        expect("unsigned image refused", outcome(plain, key) == "not signed")
        expect("unsigned image passes with --unsigned", outcome(plain, key, unsigned=True) == "ok")

        if prod_elf:
            r = subprocess.run([sys.executable, os.path.join(TOOLS, "elf2uf2-rp2350.py"), str(prod_elf), "--no-copy"],
                               env={k: v for k, v in os.environ.items() if k != "PICOCELL_SIGNING_KEY"},
                               capture_output=True, text=True)
            expect("production build refused without a key",
                   r.returncode != 0 and "PICOCELL_SIGNING_KEY" in r.stderr)
            img = sealed(prod_elf, KEY)
            info = verify.check(img, key)
            expect("production build signed with a rollback version", "rollback" in info)
    finally:
        shutil.rmtree(tmp)
    print(f"{failed} failed")
    sys.exit(failed)


if __name__ == "__main__":
    main()
