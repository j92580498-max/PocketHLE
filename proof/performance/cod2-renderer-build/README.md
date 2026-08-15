# Call of Duty 2 renderer-build selection

`COD2SOInstaller.cab` ships three builds of the game and one GL ES driver:

| File | Renderer | Imports |
|---|---|---|
| `cod2.exe` | software (PDB says `Release Software`) | no GL ES driver |
| `cod2_gles.exe` | Intel 2700G | `libGLES_CL.dll` |
| `cod2_goforce.exe` | NVIDIA GoForce | `libGLES_CM.dll` (bundled in the cabinet) |
| `libGLES_CM.dll` | — | the driver the cabinet carries |

The Start-menu shortcut points at `cod2.exe`, and the cabinet's
`SETUPDLL.999` is what makes the choice on a device: it imports
`RegOpenKeyExW`, `DeleteAndRenameFile` and `DeleteFileW`, and carries the
strings `Software\NVIDIA Corporation\GFSDK`,
`\Windows\wmv9decoder2700g.dll` and `%s\cod2.exe` / `%s\cod2_gles.exe` /
`%s\cod2_goforce.exe`. A GoForce handheld gets `cod2_goforce.exe` renamed
over `cod2.exe`; an Intel 2700G device, whose ROM carries
`wmv9decoder2700g.dll`, gets `cod2_gles.exe`; a device with neither keeps
the software build.

PocketHLE runs no install-time DLLs, so it followed the shortcut and
always launched the software build — a completely correct run in which
the GL ES layer is never called and the game rasterises every pixel in
emulated ARM code.

## Measurement

Same host, same binary, same scenario for every row: Unicorn ARM,
240x320, `--message-budget 0`, scripted key presses and taps that reach
Battle of Moscow gameplay, `--dump-frame-stride 50 --max-frames 52`, run
to `frame_counter=2551`. Steady state is measured over the second half of
the dumps, so start-up and the two input stalls are excluded. One run per
row.

| Launched build | Steady state | Per frame | Wall to frame 2550 |
|---|---:|---:|---:|
| `cod2.exe` (software, what we used to pick) | **13.7 fps** | 73.00 ms | 196.7 s |
| `cod2_goforce.exe` (`--module-path`, before the fix) | **39.1 fps** | 25.57 ms | 69.8 s |
| `COD2SOInstaller.cab` (after the fix) | **41.9 fps** | 23.88 ms | 68.3 s |

**2.9x** faster off the same cabinet, with no change to the rasterizer.

Both builds reach gameplay and both are proof frames here:
`software-build-gameplay.png` and `accelerated-build-gameplay.png` are
dump 51 (guest frame 2550) of the first and third rows. The `.ppm` files
are the raw 240x320 dumps; the `.png` files are rotated 90° so they read
right-side up, as COD2's framebuffer always needs. The camera differs
between the two — they are separate runs of a live game, not a
pixel-for-pixel A/B — so read them as "both builds are playing the same
level", not as an image diff.

## Reproducing

```sh
cargo build --release -p pocket-cli --features unicorn
pockethle run COD2SOInstaller.cab --cpu unicorn \
  --key 1:enter --key 2:enter --key 3:enter \
  --key 100:enter --key 200:enter --key 300:enter \
  --tap 420:224,73 --tap 600:224,73 --tap 800:120,160 --tap 1000:120,160 \
  --message-budget 0 --max-slices 40000000 \
  --dump-frames-to <dir> --dump-frame-stride 50 --max-frames 52
```

The first log line names the build that was picked:

```
CAB COD2SOInstaller.cab -> /tmp/pockethle-cab-XXXXXX/cod2_goforce.exe (Aspyr Media / COD2)
GetModuleFileNameW will report "\Program Files\COD2\cod2.exe"
```

To measure the software build for comparison, extract the cabinet once
and launch it by name with `--rom-dir <extracted> --module-path
'\Program Files\COD2\cod2.exe'`.

## Verification

- `cargo test --workspace` — passed. The behaviour is pinned by
  `a_cabinet_that_ships_a_hardware_renderer_build_launches_it`,
  `the_driver_the_cabinet_ships_decides_between_two_hardware_builds`,
  `a_library_entry_launches_the_hardware_renderer_build` (which also
  asserts the guest still sees `\Program Files\COD2\cod2.exe`) and
  `a_shortcut_target_without_a_renderer_sibling_is_left_alone`.
- `cargo build --release -p pocket-cli --features unicorn` — passed.

These are host CLI timings on a desktop, not a device FPS claim; the
Android FPS overlay remains the authoritative device-side measurement.
What transfers to a phone is the cause: the software build asks the CPU
to do the work the emulator would otherwise do in native Rust, and that
gap is wider on a phone, not narrower.
