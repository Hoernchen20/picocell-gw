#!/usr/bin/env python3
"""Convert an RP2350 ELF to UF2 (family RP2350_ARM_S) and copy it to a BOOTSEL drive.

Used as the cargo runner in fw/.cargo/config.toml, so `cargo run --release` builds,
converts and, if a board in BOOTSEL mode is mounted (drive "RP2350"), flashes it.
Stand-in for `picotool uf2 convert` / `picotool load`.

    elf2uf2-rp2350.py <firmware.elf> [--no-copy]
"""

import getpass
import shutil
import struct
import sys
from pathlib import Path

UF2_MAGIC_START0 = 0x0A324655
UF2_MAGIC_START1 = 0x9E5D5157
UF2_MAGIC_END = 0x0AB16F30
UF2_FLAG_FAMILY_ID_PRESENT = 0x00002000
RP2350_ARM_S_FAMILY_ID = 0xE48BFF59
PAGE = 256
FLASH_START, FLASH_END = 0x10000000, 0x12000000


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
    pages = to_pages(load_segments(elf_path.read_bytes()))
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
