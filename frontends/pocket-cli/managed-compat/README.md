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
