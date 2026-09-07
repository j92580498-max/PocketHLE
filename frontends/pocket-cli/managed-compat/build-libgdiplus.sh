#!/bin/sh
# Build and install a patched libgdiplus that honors ImageAttributes.SetColorKey
# on 24bpp (and other non-alpha) bitmaps. Upstream libgdiplus converts keyed
# pixels to "transparent white" (0x00FFFFFF) via GdipBitmapSetPixel, but on
# bitmaps without an alpha channel the alpha is dropped and the keyed pixels
# become opaque white. Managed Windows Mobile games that use color-key sprite
# transparency (e.g. vAlienAttack) then render with white halos.
#
# Requires: git, autoconf, automake, libtool, pkg-config, gettext and the
# libgdiplus build dependencies (libglib2.0-dev libcairo2-dev libgif-dev
# libtiff5-dev libexif-dev libfontconfig1-dev libfreetype6-dev libpng-dev).
set -e

workdir=${LIBGDIPLUS_WORKDIR:-/tmp/libgdiplus-build}
repo=${LIBGDIPLUS_REPO:-https://github.com/mono/libgdiplus}
here=$(cd "$(dirname "$0")" && pwd)
patch="$here/libgdiplus-colorkey.patch"

rm -rf "$workdir"
git clone --depth 1 "$repo" "$workdir"
cd "$workdir"
patch -p1 < "$patch"

./autogen.sh --prefix=/usr/local
make -j"$(nproc)"
make install
ldconfig
echo "Patched libgdiplus installed to /usr/local/lib"
