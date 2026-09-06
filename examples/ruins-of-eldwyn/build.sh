#!/bin/bash
# Cross-compile Ruins of Eldwyn for Windows CE / Pocket PC 2002 (ARMv4T).
# Toolchain: clang (ARM target) + lld-link + llvm-dlltool. No SDK needed.
set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"
LLVM_BIN="${LLVM_BIN:-/usr/lib/llvm-14/bin}"
CC="${CC:-$LLVM_BIN/clang}"
LINK="${LINK:-$LLVM_BIN/lld-link}"
DLLTOOL="${DLLTOOL:-$LLVM_BIN/llvm-dlltool}"
OBJ="$ROOT/build/obj"
EXE="$ROOT/build/eldwyn.exe"

mkdir -p "$OBJ" "$ROOT/build"

CFLAGS="--target=armv4t-none-eabi -mcpu=arm7tdmi -marm -O2 \
 -ffreestanding -fno-builtin -fno-stack-protector -fno-unwind-tables \
 -fno-asynchronous-unwind-tables -fomit-frame-pointer -fno-strict-aliasing \
 -Wall -Wno-unused-function -I$ROOT/src"

echo "[1/4] compiling guest C + import thunks -> ARM objects"
for s in "$ROOT"/src/*.c; do
    o="$OBJ/$(basename "${s%.c}").o"
    "$CC" $CFLAGS -c "$s" -o "$o"
done
"$CC" --target=armv4t-none-eabi -mcpu=arm7tdmi -marm -c "$ROOT/src/imports.s" -o "$OBJ/imports.o"
"$CC" --target=armv4t-none-eabi -mcpu=arm7tdmi -marm -c "$ROOT/src/divmod.s" -o "$OBJ/divmod.o"

echo "[2/4] linking ARM ELF (ld.lld + ldscript.ld)"
/usr/lib/llvm-14/bin/ld.lld "$OBJ"/*.o -T "$ROOT/ldscript.ld" -o "$ROOT/build/eldwyn.elf" -Map "$ROOT/build/eldwyn.map"

echo "[3/4] wrapping ELF into a Windows CE PE (tools/pelink.py)"
python3 "$ROOT/tools/pelink.py" "$ROOT/build/eldwyn.elf" "$EXE"

echo "[4/4] done: $EXE"
