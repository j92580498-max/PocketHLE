#!/usr/bin/env python3
"""Patch an ARM PE produced by lld-link into a Windows CE image.

lld-link (LLVM 14) cannot emit Subsystem=9 (WINDOWS_CE_GUI), so we link
with -subsystem:windows and rewrite the field afterwards, along with the
WinCE-typical subsystem versions (4.20 = Pocket PC 2003).
"""
import struct
import sys


def main(path):
    data = bytearray(open(path, "rb").read())
    pe = data.index(b"PE\0\0")
    coff = pe + 4
    opt = coff + 20
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic == 0x10B:      # PE32
        sub_off = opt + 68
    elif magic == 0x20B:    # PE32+
        sub_off = opt + 68
    else:
        sys.exit(f"unknown optional header magic {magic:#x}")
    struct.pack_into("<H", data, sub_off, 9)            # WINDOWS_CE_GUI
    struct.pack_into("<H", data, sub_off - 8, 4)        # major subsystem version
    struct.pack_into("<H", data, sub_off - 6, 20)       # minor subsystem version
    machine = struct.unpack_from("<H", data, coff)[0]
    open(path, "wb").write(data)
    print(f"patched {path}: machine={machine:#06x} subsystem=9 (Windows CE GUI) 4.20")


if __name__ == "__main__":
    main(sys.argv[1])
