#!/usr/bin/env python3
"""Link a statically-linked ARM ELF into a Windows CE ARM PE.

The guest is cross-compiled with clang (armv4t-none-eabi -> ELF objects)
and linked with ld.lld against ldscript.ld, which places everything at
its final VAs. This script then wraps the result in a PE32 image:

  * machine = IMAGE_FILE_MACHINE_ARM (0x01c0)
  * subsystem = WINDOWS_CE_GUI (9), version 4.20 (Pocket PC 2003)
  * entry point = WinMain (CE GUI EXEs are entered directly at WinMain)
  * import directory + IAT filled from the reserved .idata area laid
    out by src/imports.s (__iat_coredll / __iat_gles / __imp_dir)

No SDK, no CeGCC: the ELF is the single source of truth for addresses.
"""
import struct
import sys

IMAGE_BASE = 0x00012000
SECTION_ALIGN = 0x1000
FILE_ALIGN = 0x200

SHT_PROGBITS = 1
SHT_SYMTAB = 2
SHT_STRTAB = 3
SHT_NOBITS = 8

SHF_ALLOC = 0x2
SHF_EXECINSTR = 0x4
SHF_WRITE = 0x1


def parse_elf(data):
    assert data[:4] == b"\x7fELF", "not an ELF"
    (e_type, e_machine, _ver, _entry, _phoff, e_shoff, _flags, _ehsize,
     _phentsize, _phnum, e_shentsize, e_shnum, e_shstrndx) = struct.unpack_from(
        "<HHIIIIIHHHHHH", data, 16)
    secs = []
    for i in range(e_shnum):
        off = e_shoff + i * e_shentsize
        (name, typ, flags, addr, offset, size, link, _info, _align, entsz) = \
            struct.unpack_from("<IIIIIIIIII", data, off)
        secs.append(dict(name=name, typ=typ, flags=flags, addr=addr,
                         offset=offset, size=size, link=link))
    shstr = secs[e_shstrndx]
    def sname(n):
        end = data.index(b"\0", shstr["offset"] + n)
        return data[shstr["offset"] + n:end].decode()
    for s in secs:
        s["sname"] = sname(s["name"])

    # symbols (for entry point lookup)
    symtab = next((s for s in secs if s["typ"] == SHT_SYMTAB), None)
    syms = {}
    if symtab:
        strtab = secs[symtab["link"]]
        cnt = symtab["size"] // 16
        for i in range(cnt):
            off = symtab["offset"] + i * 16
            (nm, val, _sz, _info, _oth, _sh) = struct.unpack_from("<IIIBBH", data, off)
            end = data.index(b"\0", strtab["offset"] + nm)
            name = data[strtab["offset"] + nm:end].decode()
            if name:
                syms[name] = val
    return secs, syms


def collect(secs, blob):
    """Group ELF output sections into the PE's four sections."""
    text = bytearray()
    data = bytearray()
    idata = bytearray()
    bss_size = 0
    text_va = data_va = idata_va = bss_va = None

    for s in secs:
        if s["typ"] not in (SHT_PROGBITS, SHT_NOBITS) or not (s["flags"] & SHF_ALLOC):
            continue
        if s["addr"] < IMAGE_BASE:
            continue
        raw = blob[s["offset"]:s["offset"] + s["size"]] if s["typ"] == SHT_PROGBITS else b""
        if s["flags"] & SHF_EXECINSTR:
            if text_va is None:
                text_va = s["addr"]
            text += raw
        elif s["sname"] == ".idata":
            idata_va = s["addr"]
            idata += raw
        elif s["flags"] & SHF_WRITE:
            if s["typ"] == SHT_NOBITS:
                bss_va = s["addr"]
                bss_size = max(bss_size, s["size"])
            else:
                if data_va is None:
                    data_va = s["addr"]
                data += raw
        else:
            # read-only data lives in .text (already merged by the script)
            if text_va is None:
                text_va = s["addr"]
            text += raw
    return dict(text=(text_va, text), data=(data_va, data),
                idata=(idata_va, idata), bss=(bss_va, bss_size))


def build_imports(idata, idata_va, syms):
    """Fill the reserved __imp_dir area with descriptors, ILTs and
    hint/name entries. Everything is computed in image VAs and stored
    as RVAs (VA - IMAGE_BASE) where the PE format expects them.
    Returns (import_dir_rva, import_dir_size, iat_rva, iat_size)."""
    iat_core = syms["__iat_coredll"]
    iat_gles = syms["__iat_gles"]
    imp_dir = syms["__imp_dir"]
    base = imp_dir  # VA of the reserved area

    dlls = [
        ("coredll.dll", iat_core, DLL_EXPORTS["coredll.dll"]),
        ("libGLES_CM.dll", iat_gles, DLL_EXPORTS["libGLES_CM.dll"]),
    ]

    def rva(va):
        return va - IMAGE_BASE

    def w32(va, v):
        struct.pack_into("<I", idata, va - idata_va, v)

    def wstr(va, b):
        idata[va - idata_va:va - idata_va + len(b)] = b

    desc_size = (len(dlls) + 1) * 20
    cur = base + desc_size

    # hint/name blobs first so ILT entries can point at them
    hint_rvas = []
    for dll, iat, names in dlls:
        hs = []
        for n in names:
            blob = struct.pack("<H", 0) + n.encode() + b"\0\0"
            wstr(cur, blob)
            hs.append(cur)
            cur += len(blob)
        hint_rvas.append(hs)

    # ILT arrays
    ilt_rvas = []
    for i, (dll, iat, names) in enumerate(dlls):
        ilt_rvas.append(cur)
        for h in hint_rvas[i]:
            w32(cur, rva(h))
            cur += 4
        w32(cur, 0)
        cur += 4

    # dll name strings
    name_rvas = []
    for dll, iat, names in dlls:
        wstr(cur, dll.encode() + b"\0")
        name_rvas.append(cur)
        cur += len(dll) + 1

    # descriptors
    d = base
    for i, (dll, iat, names) in enumerate(dlls):
        w32(d, rva(ilt_rvas[i]))       # OriginalFirstThunk
        w32(d + 4, 0)                  # TimeDateStamp
        w32(d + 8, 0)                  # ForwarderChain
        w32(d + 12, rva(name_rvas[i])) # Name
        w32(d + 16, rva(iat))          # FirstThunk (IAT laid out by imports.s)
        d += 20
    w32(d, 0); w32(d + 4, 0); w32(d + 8, 0); w32(d + 12, 0); w32(d + 16, 0)

    return rva(imp_dir), desc_size, rva(iat_core), (iat_gles + 48 * 4) - iat_core


def dlls_name(dll, j):
    return DLL_EXPORTS[dll][j]


def ilt_total(sizes, upto):
    return sum(sizes)


DLL_EXPORTS = {
    "coredll.dll": [
        "ExitProcess", "GetTickCount", "Sleep", "GetAsyncKeyState",
        "RegisterClassW", "CreateWindowExW", "ShowWindow", "UpdateWindow",
        "DefWindowProcW", "PeekMessageW", "DispatchMessageW",
        "PostQuitMessage", "DestroyWindow", "MessageBoxW", "waveOutOpen",
        "waveOutPrepareHeader", "waveOutUnprepareHeader", "waveOutWrite",
        "waveOutReset", "waveOutClose",
    ],
    "libGLES_CM.dll": [
        "eglGetDisplay", "eglInitialize", "eglChooseConfig",
        "eglCreateWindowSurface", "eglCreateContext", "eglMakeCurrent",
        "eglSwapBuffers", "eglTerminate", "eglGetError", "glEnable",
        "glDisable", "glClear", "glClearColor", "glMatrixMode",
        "glLoadIdentity", "glLoadMatrixf", "glFrustumf", "glOrthof",
        "glViewport", "glFogf", "glFogfv", "glHint", "glVertexPointer",
        "glColorPointer", "glTexCoordPointer", "glEnableClientState",
        "glDisableClientState", "glDrawArrays", "glDrawElements",
        "glBindTexture", "glGenTextures", "glTexImage2D", "glTexParameterf",
        "glTexEnvf", "glBlendFunc", "glAlphaFunc", "glDepthFunc",
        "glDepthMask", "glShadeModel", "glCullFace", "glColor4f",
        "glGetError",
    ],
}


def build_pe(img, syms, elf_entry):
    text_va, text = img["text"]
    data_va, data = img["data"]
    idata_va, idata = img["idata"]
    bss_va, bss_size = img["bss"]

    import_rva, import_size, iat_rva, iat_size = build_imports(
        idata, idata_va, syms)

    def align_up(v, a):
        return (v + a - 1) // a * a

    # first pass: decide which sections make it into the image
    wanted = []
    for name, va, raw, chars in [
        (b".text", text_va, text, 0x60000020),
        (b".data", data_va, data, 0xC0000040),
        (b".idata", idata_va, idata, 0xC0000040),
        (b".bss", bss_va, b"", 0xC0000080),
    ]:
        if va is None or (name != b".bss" and not raw):
            continue
        wanted.append((name, va, raw, chars))

    nsecs = len(wanted)
    hdr_size = 0x80 + 4 + 20 + 224 + nsecs * 40
    hdr_size = (hdr_size + FILE_ALIGN - 1) // FILE_ALIGN * FILE_ALIGN

    secs = []
    off = 0x80 + hdr_size   # DOS stub precedes the PE headers
    for name, va, raw, chars in wanted:
        vsize = len(raw)
        if name == b".bss":
            vsize = bss_size
        rsize = 0 if name == b".bss" else align_up(len(raw), FILE_ALIGN)
        secs.append((name, va, vsize, rsize, off, chars, raw))
        off += rsize

    max_end = max(va + align_up(vs, SECTION_ALIGN) for _, va, vs, *_ in secs)
    size_of_image = align_up(max_end - IMAGE_BASE, SECTION_ALIGN)

    opt = struct.pack(
        "<HBBIIIIIIIIIHHHHHHIIIIHHIIIIII",
        0x10B,          # PE32 magic
        8, 0,           # linker version
        align_up(len(text), FILE_ALIGN),            # SizeOfCode
        align_up(len(data) + len(idata), FILE_ALIGN),  # SizeOfInitializedData
        bss_size,       # SizeOfUninitializedData
        elf_entry - IMAGE_BASE,   # AddressOfEntryPoint
        text_va - IMAGE_BASE,     # BaseOfCode
        (data_va or idata_va) - IMAGE_BASE,       # BaseOfData
        IMAGE_BASE,
        SECTION_ALIGN,
        FILE_ALIGN,
        3, 0,           # OS version 3.00 (WinCE 3)
        0, 0,           # image version
        4, 20,          # subsystem version 4.20 (Pocket PC 2003)
        0,              # Win32 version value
        size_of_image,
        0x80 + hdr_size,  # SizeOfHeaders (DOS stub + PE headers)
        0,              # checksum (patched below)
        9,              # subsystem: WINDOWS_CE_GUI
        0,              # dll characteristics
        0x10000, 0x1000,  # stack reserve/commit
        0, 0,           # heap reserve/commit (CE: none)
        0,              # loader flags
        16,             # NumberOfRvaAndSizes
    )
    # data directories (16 * 8)
    dirs = [b"\0" * 8] * 16
    dirs[1] = struct.pack("<II", import_rva, import_size)
    dirs[12] = struct.pack("<II", iat_rva, iat_size)
    opt += b"".join(dirs)
    assert len(opt) == 224, len(opt)

    pe = b"PE\0\0" + struct.pack(
        "<HHIIIHH", 0x01C0, nsecs, 0, 0, 0, 224, 0x0102) + opt
    for name, va, vsize, rsize, roff, chars, raw in secs:
        pe += struct.pack("<8sIIIIIIHHI", name, max(vsize, 1),
                          va - IMAGE_BASE, rsize, roff, 0, 0, 0, 0, chars)
    pe += b"\0" * (hdr_size - len(pe))

    out = bytearray(b"MZ" + b"\0" * 62)
    struct.pack_into("<I", out, 0x3C, 0x80)
    out += b"\0" * (0x80 - len(out))
    out += pe
    for name, va, vsize, rsize, roff, chars, raw in secs:
        if name == b".idata":
            print("DBG idata written:", raw[:20].hex(), file=sys.stderr)
        if raw:
            out += raw + b"\0" * (rsize - len(raw))

    # PE checksum
    total = len(out)
    chk = 0
    for i in range(0, total, 2):
        w = out[i] | (out[i + 1] << 8 if i + 1 < total else 0)
        chk += w
        chk = (chk & 0xFFFF) + (chk >> 16)
    chk = (chk + total) & 0xFFFF
    # write into optional header checksum field
    coff_off = 0x80 + 4
    opt_off = coff_off + 20
    struct.pack_into("<I", out, opt_off + 64, chk)
    return bytes(out)


def main():
    elf_path, pe_path = sys.argv[1], sys.argv[2]
    data = open(elf_path, "rb").read()
    secs, syms = parse_elf(data)
    img = collect(secs, data)
    entry = syms.get("WinMain")
    if entry is None:
        sys.exit("WinMain symbol not found in ELF")
    pe = build_pe(img, syms, entry)
    open(pe_path, "wb").write(pe)
    text_va, text = img["text"]
    print(f"pelink: {pe_path}  entry={entry:#x}  .text={len(text)}B @ {text_va:#x} "
          f".data={len(img['data'][1])}B  .idata={len(img['idata'][1])}B  "
          f".bss={img['bss'][1]}B")


if __name__ == "__main__":
    main()
