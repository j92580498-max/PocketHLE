# MetalStrike Windows Mobile diagnostic proof

This directory records the supplied `MetalStrike-0581f392e0fc.exe` probe.

## Verified findings

- The original run reached the registered window procedure but crashed in `WM_CREATE` with `READ_UNMAPPED` at guest address `0x00000004`; `frame_counter=0`.
- The cause was PocketHLE fabricating a non-null zero-filled `GWL_USERDATA` pointer before `WM_CREATE`. MetalStrike treats the field as its `nGameX` object pointer, reads its vtable at offset zero, and then dereferences the vtable entry at offset `+4`.
- The fix leaves `GWL_USERDATA` at its Windows-initialized value of zero until the guest installs its own object with `SetWindowLongW`.
- The supplied executable also imports `nGameX2K3.dll` (not only `ngamex.dll`) and requires the `GetSurface`, screen-size, and key-list symbols. Those handlers are now registered and the surface is mapped to PocketHLE's framebuffer.

## Verification run

Build:

```text
cargo build --release -p pocket-cli --features unicorn
```

Focused tests:

```text
cargo test -p pocket-winceapi --no-default-features
cargo test -p pocket-kernel --no-default-features
```

Both passed: 82 WinCE API tests and 83 kernel tests. The full workspace test suite also passed after installing the host GTK/GLib development packages; the only ignored tests are the three CPU microbenchmarks.

The fixed executable probe reached the render loop and produced 20 framebuffer captures:

```text
Final framebuffer snapshot written to /tmp/pockethle-final.ppm (230415 bytes, frame_counter=581)
Emulator exited cleanly.
```

The supplied executable is missing its external `data.pkg` / `data.npk` resource files. Consequently this payload does not reach the actual MetalStrike gameplay scene: after the window and render loop are fixed, it reports `CreateFileW("\\Program Files\\Game\\data.ndx") -> INVALID_HANDLE_VALUE` and shows the game's installation-error screen. The capture is included as `baseline-after-window-fix.png` / `baseline-after-window-fix.ppm` to prove that the original `frame_counter=0` startup failure is fixed and pixels now reach the framebuffer.

A run through `tools/ai-tap-sequence.py` cannot honestly be marked as gameplay PASS with this incomplete payload; it would only confirm the same installation-error screen. A Windows Mobile device/emulator is not available in this Linux environment, so the proof is PocketHLE's ARM Unicorn framebuffer, not native-device output.

## Requested tap-sequence output

The repository helper was run with two taps, `(120,160)` and `(120,220)`, and `--message-budget 0`. It exited with status 0 and captured 20 frames; the exact output is in `ai-tap-sequence.log` and raw captures are in `ai-tap-frames/`:

```text
Final framebuffer snapshot written to /tmp/pockethle-final.ppm (230415 bytes, frame_counter=585)
Emulator exited cleanly.
```

This is an emulator/render-loop PASS, not a gameplay PASS, because the uploaded EXE does not include its required external data package.
