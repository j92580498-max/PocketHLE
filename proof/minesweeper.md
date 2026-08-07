# MineSweeper (Windows Mobile) rendering proof

This proof uses the uploaded native ARM Windows Mobile CAB `MineSweeper.ppc30_arm.CAB` from the China Starsoft release.

## Failure and fix

The executable imports `coredll.dll!GetUpdateRect` and calls it immediately before `BeginPaint` in its main window procedure. PocketHLE had the ordinal (`274`) in the clean-room WinCE table but had not registered a handler, so the synthetic `WM_PAINT` was discarded before the game entered its drawing code. The framebuffer remained blank.

The fix registers `GetUpdateRect` and returns the current emulated screen rectangle `{0, 0, width, height}`. This matches the existing `GetClientRect` / `BeginPaint` model and lets the guest continue through its real paint path. The handler safely accepts a null `LPRECT`, as the Windows API does for callers that only query whether an update exists.

## Reproduction

```sh
target/release/pockethle run MineSweeper.ppc30_arm.CAB \
  --cpu unicorn \
  --dump-frames-to /tmp/mine-final-frames \
  --max-frames 8 \
  --message-budget 5000 \
  --max-slices 8000000
```

The final run exited cleanly with `frame_counter=15`. Its API trace contains successful `GetUpdateRect`, `BeginPaint`, `LoadBitmapW`, and seven `BitBlt` calls. Eight RGB565 framebuffer snapshots were produced at 240×320.

## Screenshots

- `minesweeper-gameplay.png` — the first rendered board frame, including the board grid.
- `minesweeper-gameplay-final.png` — the later rendered game frame with the counter, smiley control, mine counter, and board.

These are emulator-side screenshots produced by the ARM Unicorn run. A physical Windows Mobile device/emulator is not available in this environment, so they are not screenshots from native hardware.
