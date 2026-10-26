#!/usr/bin/env python3
"""Seal an RP2350 ELF, convert it to UF2 (family RP2350_ARM_S) and copy it to a BOOTSEL
drive.

Used as the cargo runner in fw/.cargo/config.toml, so `cargo run --release` builds,
seals, converts and, if a board in BOOTSEL mode is mounted (drive "RP2350"), flashes
it. Sealing (`picotool seal --hash`, sketch.md section 8.1) adds the SHA-256 and the
major/minor version from fw/Cargo.toml as a second IMAGE_DEF. With $PICOCELL_SIGNING_KEY
(a secp256k1 PEM, e.g. fw/keys/test-signing.pem) the image is also signed and then
checked with tools/verify-image.py against that key. A production build (`--features
production`, marked by the symbol PICOCELL_PRODUCTION) is refused without a key and gets
the rollback version from fw/Cargo.toml; dev builds have none, so buying one never
writes the OTP rollback rows. picotool is taken from $PICOTOOL, the PATH or
~/.local/bin.

    elf2uf2-rp2350.py <firmware.elf> [--no-copy]
"""

import getpass
import os
import re
import shutil
import subprocess
import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.dont_write_bytecode = True

UF2_MAGIC_START0 = 0x0A324655
UF2_MAGIC_START1 = 0x9E5D5157
UF2_MAGIC_END = 0x0AB16F30
UF2_FLAG_FAMILY_ID_PRESENT = 0x00002000
RP2350_ARM_S_FAMILY_ID = 0xE48BFF59
PAGE = 256
FLASH_START, FLASH_END = 0x10000000, 0x12000000
CARGO_TOML = Path(__file__).resolve().parent.parent / "fw/Cargo.toml"


def picotool():
    found = os.environ.get("PICOTOOL") or shutil.which("picotool")
    if not found and (Path.home() / ".local/bin/picotool").is_file():
        found = str(Path.home() / ".local/bin/picotool")
    if not found:
        sys.exit("picotool not found: install it (fw/README.md) or set PICOTOOL")
    return found


def has_symbol(elf: bytes, name: bytes) -> bool:
    """Whether the ELF's symbol table has `name`."""
    e_shoff, = struct.unpack_from("<I", elf, 0x20)
    e_shentsize, e_shnum = struct.unpack_from("<HH", elf, 0x2E)
    sections = [struct.unpack_from("<10I", elf, e_shoff + i * e_shentsize) for i in range(e_shnum)]
    for _name, sh_type, _flags, _addr, offset, size, link, _info, _align, entsize in sections:
        if sh_type != 2:  # SHT_SYMTAB
            continue
        strtab = sections[link][4]
        for o in range(offset, offset + size, entsize):
            st_name, = struct.unpack_from("<I", elf, o)
            start = strtab + st_name
            if elf[start : elf.index(b"\0", start)] == name:
                return True
    return False


def seal(elf_path: Path) -> Path:
    """Writes <elf>.sealed.elf with hash, version and, with a key, signature; returns its
    path."""
    toml = CARGO_TOML.read_text()
    m = re.search(r'^version = "(\d+)\.(\d+)\.\d+"', toml, re.M)
    if not m:
        sys.exit(f"no version in {CARGO_TOML}")
    production = has_symbol(elf_path.read_bytes(), b"PICOCELL_PRODUCTION")
    key = os.environ.get("PICOCELL_SIGNING_KEY")
    if production and not key:
        sys.exit("production build: set PICOCELL_SIGNING_KEY to the signing key (.pem)")
    out = elf_path.with_name(elf_path.name + ".sealed.elf")
    cmd = [picotool(), "seal", "--quiet", "--hash", *(["--sign"] if key else []),
           str(elf_path), "-t", "elf", str(out), "-t", "elf", *([key] if key else []),
           "--major", m[1], "--minor", m[2]]
    if production:
        r = re.search(r"^\[package\.metadata\.picocell\][^\[]*?^rollback = (\d+)", toml, re.M | re.S)
        if not r:
            sys.exit(f"no rollback version in {CARGO_TOML} ([package.metadata.picocell])")
        cmd += ["--rollback", r[1]]
    if subprocess.run(cmd).returncode != 0:
        sys.exit("picotool seal failed")
    if key:
        verify = __import__("verify-image")
        try:
            info = verify.check(flat(to_pages(load_segments(out.read_bytes()))), verify.public_key(key))
        except verify.Invalid as e:
            sys.exit(f"{out}: {e}")
        print(f"signed ({'production' if production else 'dev'} build), boot key hash {info['boot_key_hash']}")
    return out


def load_segments(elf: bytes):
    """Yield (physical address, data) of loadable segments with file contents."""
    if elf[:4] != b"\x7fELF" or elf[4] != 1 or elf[5] != 1:
        sys.exit("not a 32-bit little-endian ELF")
    e_phoff, = struct.unpack_from("<I", elf, 0x1C)
    e_phentsize, e_phnum = struct.unpack_from("<HH", elf, 0x2A)
    for i in range(e_phnum):
        p_type, p_offset, _vaddr, p_paddr, p_filesz, _memsz, _flags, _align = struct.unpack_from(
            "<8I", elf, e_phoff + i * e_phentsize
        )
        if p_type == 1 and p_filesz > 0:  # PT_LOAD
            yield p_paddr, elf[p_offset : p_offset + p_filesz]


def to_pages(segments):
    pages = {}
    for addr, data in segments:
        if not (FLASH_START <= addr and addr + len(data) <= FLASH_END):
            sys.exit(f"segment at {addr:#010x} is outside flash")
        for i, byte in enumerate(data):
            a = addr + i
            page = pages.setdefault(a & ~(PAGE - 1), bytearray(PAGE))
            page[a % PAGE] = byte
    return sorted(pages.items())


def flat(pages) -> bytes:
    """The pages as flash contents from FLASH_START on, gaps as 0xff."""
    if pages[0][0] != FLASH_START:
        sys.exit(f"image does not start at {FLASH_START:#x}")
    out = bytearray(b"\xff" * (pages[-1][0] + PAGE - FLASH_START))
    for addr, data in pages:
        out[addr - FLASH_START : addr - FLASH_START + len(data)] = data
    return bytes(out)


def to_uf2(pages) -> bytes:
    out = bytearray()
    for n, (addr, data) in enumerate(pages):
        block = struct.pack(
            "<8I",
            UF2_MAGIC_START0,
            UF2_MAGIC_START1,
            UF2_FLAG_FAMILY_ID_PRESENT,
            addr,
            PAGE,
            n,
            len(pages),
            RP2350_ARM_S_FAMILY_ID,
        )
        block += bytes(data) + bytes(476 - PAGE) + struct.pack("<I", UF2_MAGIC_END)
        assert len(block) == 512
        out += block
    return bytes(out)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1:
        sys.exit(__doc__)
    elf_path = Path(args[0])
    pages = to_pages(load_segments(seal(elf_path).read_bytes()))
    uf2_path = elf_path.with_suffix(".uf2")
    uf2_path.write_bytes(to_uf2(pages))
    print(f"{uf2_path}: {len(pages)} blocks, {len(pages) * PAGE // 1024} KiB")

    if "--no-copy" in sys.argv:
        return
    user = getpass.getuser()
    drives = [d for d in (Path(f"/media/{user}/RP2350"), Path(f"/run/media/{user}/RP2350")) if d.is_dir()]
    if not drives:
        print("no RP2350 drive mounted: hold BOOTSEL while plugging in USB, or copy the .uf2 by hand")
        return
    shutil.copy(uf2_path, drives[0] / uf2_path.name)
    print(f"copied to {drives[0]}; the board reboots into the new firmware")


if __name__ == "__main__":
    main()
