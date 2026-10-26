#!/usr/bin/env python3
"""Check a sealed RP2350 image the way the boot ROM does (sketch.md section 8.1, 8c).

    tools/verify-image.py <image.elf|.uf2|.bin> [--key PEM] [--unsigned]

Finds the IMAGE_DEF in the image's block loop, recomputes the SHA-256 over the load
map and the block (as picotool seal and the ROM do: the TBYB flag cleared, so the hash
still matches after explicit_buy), compares it with the HASH_VALUE item, checks the
secp256k1 signature and that its public key is the one from --key (default: the test
key fw/keys/test-signing.pem; private or public PEM). Prints version, rollback version
and the boot key hash that goes into OTP BOOTKEYn. --unsigned accepts an image with a
hash and no signature (dev builds).

Independent of picotool's own check: the ECDSA verification is plain Python, the key
is read with openssl. Exits 0 when everything matches, 1 otherwise.
"""

import argparse
import hashlib
import os
import struct
import subprocess
import sys

TOOLS = os.path.dirname(os.path.abspath(__file__))
TEST_KEY = os.path.join(os.path.dirname(TOOLS), "fw/keys/test-signing.pem")

FLASH_START = 0x10000000
BLOCK_START, BLOCK_END = 0xFFFFDED3, 0xAB123579
# Block item types (pico-sdk boot/picobin.h); bit 7 set: two-byte size.
IMAGE_DEF, VECTOR_TABLE, ENTRY_POINT, LOAD_MAP = 0x42, 0x03, 0x44, 0x06
HASH_DEF, VERSION, SIGNATURE, HASH_VALUE, LAST = 0x47, 0x48, 0x09, 0x4B, 0xFF
IMAGE_TYPE_TBYB = 0x8000
HASH_SHA256, SIGNATURE_SECP256K1 = 1, 1

# secp256k1
P = 2**256 - 2**32 - 977
N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
G = (
    0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798,
    0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8,
)
SECP256K1_OID = bytes.fromhex("06052b8104000a")


class Invalid(Exception):
    pass


def _add(a, b):
    if a is None:
        return b
    if b is None:
        return a
    if a[0] == b[0] and (a[1] + b[1]) % P == 0:
        return None
    if a == b:
        m = 3 * a[0] * a[0] * pow(2 * a[1], -1, P)
    else:
        m = (b[1] - a[1]) * pow(b[0] - a[0], -1, P)
    x = (m * m - a[0] - b[0]) % P
    return x, (m * (a[0] - x) - a[1]) % P


def _mul(k, point):
    out = None
    while k:
        if k & 1:
            out = _add(out, point)
        point = _add(point, point)
        k >>= 1
    return out


def ecdsa_verify(public: bytes, digest: bytes, sig: bytes) -> bool:
    """secp256k1 ECDSA over a SHA-256 digest; key x||y and signature r||s, big-endian."""
    q = (int.from_bytes(public[:32], "big"), int.from_bytes(public[32:], "big"))
    if not (0 <= q[0] < P and 0 <= q[1] < P) or (q[1] ** 2 - q[0] ** 3 - 7) % P != 0:
        return False
    r, s = int.from_bytes(sig[:32], "big"), int.from_bytes(sig[32:], "big")
    if not (0 < r < N and 0 < s < N):
        return False
    w = pow(s, -1, N)
    x = _add(_mul(int.from_bytes(digest, "big") * w % N, G), _mul(r * w % N, q))
    return x is not None and x[0] % N == r


def public_key(pem_path) -> bytes:
    """The 64-byte x||y public key of a secp256k1 PEM key (private or public)."""
    for pubin in ([], ["-pubin"]):
        r = subprocess.run(["openssl", "pkey", *pubin, "-in", pem_path, "-pubout", "-outform", "DER"],
                           capture_output=True)
        if r.returncode == 0:
            der = r.stdout
            if SECP256K1_OID not in der or der[-65] != 0x04:
                raise Invalid(f"{pem_path}: not an uncompressed secp256k1 key")
            return der[-64:]
    raise Invalid(f"{pem_path}: openssl cannot read the key")


def flat_from_file(path) -> bytes:
    """Flash contents from FLASH_START on, gaps as 0xff."""
    data = open(path, "rb").read()
    if data[:4] == b"\x7fELF":
        sys.dont_write_bytecode = True
        sys.path.insert(0, TOOLS)
        elf2uf2 = __import__("elf2uf2-rp2350")
        return elf2uf2.flat(elf2uf2.to_pages(elf2uf2.load_segments(data)))
    if data[:8] == struct.pack("<2I", 0x0A324655, 0x9E5D5157):
        blocks = [struct.unpack_from("<8I", data, o) + (data[o + 32 : o + 32 + 256],)
                  for o in range(0, len(data), 512)]
        end = max(b[3] + b[4] for b in blocks)
        out = bytearray(b"\xff" * (end - FLASH_START))
        for b in blocks:
            out[b[3] - FLASH_START : b[3] - FLASH_START + b[4]] = b[8][: b[4]]
        return bytes(out)
    return data


def items(img, start):
    """(type, offset, size in words) of the items of the block at `start`; then the
    offset of the next block."""
    word = lambda o: struct.unpack_from("<I", img, o)[0]
    o = start + 4
    found = []
    while True:
        if o + 4 > len(img):
            raise Invalid(f"block at {start:#x} runs past the image")
        t = img[o]
        size = struct.unpack_from("<H", img, o + 1)[0] if t & 0x80 else img[o + 1]
        if t == LAST:
            nxt = start + struct.unpack_from("<i", img, o + 4)[0]
            if word(o + 8) != BLOCK_END:
                raise Invalid(f"block at {start:#x} has no end marker")
            return found, nxt
        if size == 0:
            raise Invalid(f"zero-size item at {o:#x}")
        found.append((t, o, size))
        o += 4 * size


def blocks(img):
    """All blocks of the loop: the first one starts in the first 4 KB."""
    first = next((o for o in range(0, min(len(img), 4096), 4)
                  if struct.unpack_from("<I", img, o)[0] == BLOCK_START), None)
    if first is None:
        raise Invalid("no block in the first 4 KB")
    out, o = [], first
    while True:
        its, nxt = items(img, o)
        out.append((o, its))
        if nxt == first:
            return out
        if not 0 <= nxt < len(img) or len(out) > 16 or struct.unpack_from("<I", img, nxt)[0] != BLOCK_START:
            raise Invalid(f"block loop broken after {o:#x}")
        o = nxt


def check(img: bytes, key=None, unsigned=False) -> dict:
    """Verifies the image; returns its version info. `key`: expected x||y public key."""
    # The ROM takes the last IMAGE_DEF in the loop; the firmware's own one (boot.rs) comes
    # first and picotool appends the sealed one.
    defs = [(o, its) for o, its in blocks(img) if its and its[0][0] == IMAGE_DEF]
    if not defs:
        raise Invalid("no IMAGE_DEF block")
    start, its = defs[-1]
    by_type = {t: (o, size) for t, o, size in its}
    word = lambda o: struct.unpack_from("<I", img, o)[0]
    info = {"block": FLASH_START + start}

    if VERSION in by_type:
        o, size = by_type[VERSION]
        rows = img[o + 3]
        v = word(o + 4)
        info["version"] = f"{v >> 16}.{v & 0xFFFF}"
        if rows:
            info["rollback"] = word(o + 8) & 0xFFFF

    if LOAD_MAP not in by_type or HASH_DEF not in by_type:
        raise Invalid("no load map or hash definition: not sealed")
    o, _ = by_type[LOAD_MAP]
    if img[o + 3] & 0x80:
        raise Invalid("absolute load map not supported")
    to_hash = bytearray()
    for e in range(img[o + 3] & 0x7F):
        rel, runtime, size = struct.unpack_from("<3I", img, o + 4 + 12 * e)
        if rel == 0:
            to_hash += struct.pack("<I", size)  # cleared at load
            continue
        storage = (o + rel) & 0xFFFFFFFF
        if storage + size > len(img):
            raise Invalid(f"load map entry {e} at {FLASH_START + storage:#x} is outside the image")
        to_hash += img[storage : storage + size]
    o, _ = by_type[HASH_DEF]
    if img[o + 3] != HASH_SHA256:
        raise Invalid(f"hash type {img[o + 3]}, expected SHA-256")
    block = bytearray(img[start : start + 4 * (word(o + 4) & 0xFFFF)])
    if word(start + 4) & (IMAGE_TYPE_TBYB << 16):
        block[7] &= 0x7F
    digest = hashlib.sha256(to_hash + block).digest()
    info["sha256"] = digest.hex()

    if HASH_VALUE in by_type:
        o, size = by_type[HASH_VALUE]
        stored = img[o + 4 : o + 4 * size]
        if stored != digest[: len(stored)]:
            raise Invalid("hash mismatch: the image was changed after sealing")
    elif SIGNATURE not in by_type:
        raise Invalid("neither hash value nor signature")

    if SIGNATURE not in by_type:
        if not unsigned:
            raise Invalid("not signed")
        return info
    o, size = by_type[SIGNATURE]
    if img[o + 3] != SIGNATURE_SECP256K1 or size != 33:
        raise Invalid("signature item is not secp256k1")
    public, sig = img[o + 4 : o + 68], img[o + 68 : o + 132]
    if not ecdsa_verify(public, digest, sig):
        raise Invalid("bad signature")
    info["boot_key_hash"] = hashlib.sha256(public).hexdigest()
    if key is not None and public != key:
        raise Invalid("signed with another key")
    return info


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("image")
    ap.add_argument("--key", default=TEST_KEY, help="expected signing key (default: the test key)")
    ap.add_argument("--unsigned", action="store_true", help="accept an image with a hash only")
    a = ap.parse_args()
    try:
        info = check(flat_from_file(a.image), public_key(a.key), a.unsigned)
    except Invalid as e:
        sys.exit(f"{a.image}: {e}")
    print(f"{a.image}: ok")
    for k, v in info.items():
        print(f"  {k:14} {v:#x}" if k == "block" else f"  {k:14} {v}")


if __name__ == "__main__":
    main()
