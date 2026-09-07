# Managed compatibility assemblies

Host runtimes such as Mono do not ship the .NET Compact Framework's
`Microsoft.WindowsCE.Forms` assembly, so managed Windows Mobile games that
reference it (for `SystemSettings.ScreenOrientation`, `InputPanel`, …) die
with a `TypeLoadException` before their first window opens.

This directory holds a minimal clean-room shim, `Microsoft.WindowsCE.Forms.dll`,
built from source here. `pocket-cli` adds this directory to `MONO_PATH`
automatically when launching a managed image, so the shim is picked up
without any extra flags.

The shim is intentionally a stub: on a desktop host there is no device
orientation to change, so `ScreenOrientation` reports `Angle0` and the
setter is a no-op. Games only need the types to resolve so their forms can
construct and run under normal `System.Windows.Forms`.

## Rebuilding

Requires `mcs` (mono-devel / mono-mcs):

```sh
cd frontends/pocket-cli/managed-compat
mcs -target:library -out:Microsoft.WindowsCE.Forms.dll Microsoft.WindowsCE.Forms.cs
```

The committed DLL matches the source here; rebuild only after changing the
source. New shims for other Compact Framework-only assemblies
(`Microsoft.WindowsMobile.*`, …) can be added to this directory the same
way — they are all placed on `MONO_PATH` together.

## Sprite color-key transparency (libgdiplus patch)

Mono's `System.Drawing` uses libgdiplus, whose `ImageAttributes.SetColorKey`
implementation writes keyed pixels as "transparent white" (alpha 0) through
`GdipBitmapSetPixel`. On bitmaps without an alpha channel (24bppRgb, which is
how PNG sprites without alpha decode) the alpha is dropped and the keyed
pixels render as opaque white, giving sprites white halos.

`libgdiplus-colorkey.patch` fixes this by converting the attribute-processed
bitmap clone to 32bppARGB before the color-key pass when the source format
has no alpha. Build and install the patched library with:

```sh
frontends/pocket-cli/managed-compat/build-libgdiplus.sh
```

It installs to `/usr/local/lib` (and refreshes the ld cache), which the
dynamic loader prefers over the distro package. Games using color-key
transparency must run against a libgdiplus that includes this fix.

## Sprite color-key transparency (libgdiplus patch)

Managed games draw their sprites with `ImageAttributes.SetColorKey`
(e.g. vAlienAttack keys out black). libgdiplus implements the key by
writing alpha-0 pixels into a clone of the source bitmap — but when the
source has no alpha channel (24bppRgb PNGs, the common case for these
games) the alpha is dropped and the keyed pixels are drawn as opaque
white, surrounding every sprite with a white halo.

`libgdiplus-colorkey.patch` fixes `gdip_process_bitmap_attributes` to
convert the clone to 32bppARGB before the key is applied. Build and
install the patched library with:

```sh
frontends/pocket-cli/managed-compat/build-libgdiplus.sh
```

The script clones mono/libgdiplus, applies the patch, builds and
installs to `/usr/local/lib` (run `ldconfig` afterwards — done by the
script). Requires the usual build dependencies
(`libglib2.0-dev libcairo2-dev libgif-dev libtiff5-dev libexif-dev
libfontconfig1-dev libfreetype6-dev libpng-dev autoconf automake
libtool pkg-config gettext`). Without the patch the games still run,
but sprites show white halos.
