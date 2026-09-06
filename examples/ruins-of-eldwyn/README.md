# Ruins of Eldwyn

A small top-down 3D PS1-style RPG for Windows CE / Pocket PC 2002, built for PocketHLE.

## Run

```sh
cargo run -p pocket-cli -- run RuinsOfEldwyn.cab --cpu unicorn
```

The CAB contains `eldwyn.exe`. The game targets ARMv4T, uses a 320x240 RGB565 framebuffer, OpenGL ES 1.x, 8-bit paletted textures, fixed-point math, and PCM beeps through `waveOut*`.

Controls: arrows move, A attacks/interacts, B drinks a potion.

## Build

Run `./build.sh` on a Linux host with the LLVM 14 tools used by the repository. The script compiles the guest with no CRT, links the ARM image, and wraps it into a Windows CE PE. `tools/pelink.py` provides the minimal PE/CE import directory because the host linker does not emit the legacy Windows CE ARM format directly.

## PocketHLE changes

This example relies on two emulator fixes:

- `pocket-gles` decodes OES `GL_PALETTE4_*` and `GL_PALETTE8_*` uploads inline as palette + index bytes.
- `pocket-winceapi` calculates the byte length of those palette uploads before reading guest memory; previously the upload was read as zero bytes and sampled as an incomplete white/transparent texture.

The changes are intentionally generic and are used by any guest that uploads the corresponding OpenGL ES paletted formats.
