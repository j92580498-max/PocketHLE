# MineSweeper Windows Mobile rendering proof

## Root cause

The ARM CAB imports `coredll.dll!GetUpdateRect` (ordinal 274) and calls it from its `WM_PAINT` handler before `BeginPaint`. PocketHLE had the ordinal in the WinCE export table but had not registered a handler. The call therefore followed the unimplemented API path, the paint handler discarded the update, and `frame_counter` remained zero.

## Fix

`crates/pocket-winceapi/src/coredll.rs` now registers `GetUpdateRect` and returns the current emulated screen rectangle `{0, 0, width, height}`. A null `LPRECT` is also accepted, matching the Windows API query form. This lets the existing `WM_PAINT` → `BeginPaint` → GDI `BitBlt` path run normally.

## Verification

The full workspace test suite passed: 1 + 6 + 3 + 3 + 2 + 105 + 77 + 19 + 4 + 69 tests, plus documentation tests; three CPU microbenchmarks remained intentionally ignored.

The ARM run used the supplied `MineSweeper.ppc30_arm-0cf933e540ef.CAB`:

```text
Final framebuffer snapshot written to /tmp/pockethle-final.ppm (230415 bytes, frame_counter=15)
Emulator exited cleanly.
```

The API trace counted `GetUpdateRect=1`, `BeginPaint=1`, `LoadBitmapW=1`, and `BitBlt=7`. Eight 240x320 PPM frames were produced. `gameplay-first.ppm` and `gameplay-final.ppm` are emulator framebuffer screenshots; they contain non-black pixels and the final frame has the rendered board/control region. The same ARM CAB was also run through the MIPS backend with the same two taps; it exited cleanly with `frame_counter=15` and produced eight frames.

The requested tap helper was run with two taps at `(120,160)` and `(120,220)`:

```text
Queued synthetic tap at (120,160)
Queued synthetic tap at (120,220)
Final framebuffer snapshot written to /tmp/pockethle-final.ppm (230415 bytes, frame_counter=23)
Emulator exited cleanly.
```

The machine-readable API trace is in `api-trace.jsonl`; it records the successful `GetUpdateRect` → `BeginPaint` → seven `BitBlt` calls. `tap-sequence-contact.png` is a contact sheet of the eight captured frames from the two-tap run. `final-tap-sequence.png` is a contact sheet of the 12 captured frames from the later two-tap run.

The repository does not provide a Windows Mobile Device Emulator or physical device in this Linux environment. The screenshots are therefore PocketHLE emulator framebuffer captures, not photographs of native hardware.
