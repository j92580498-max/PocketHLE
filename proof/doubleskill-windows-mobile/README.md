# DoubleSkill — Windows Mobile

## Fix

DoubleSkill reached its `WM_PAINT` handler and executed `CreateBitmap(240, 320, 1, 16)`, `SetBitmapBits`, `SelectObject`, and `BitBlt`. `SetBitmapBits` was the only imported call without a handler, so the bitmap stayed zero-filled and every screen update copied a black surface. The fix adds a real `SetBitmapBits` handler that copies the guest's RGB565 buffer into the tracked GDI bitmap, bounded by the bitmap size.

## Verification

The supplied `DoubleSkill_v.0.1-9531356b0134.cab` was run through the ARM Unicorn backend with the repository's `tools/ai-tap-sequence.py` helper:

```text
python3 tools/ai-tap-sequence.py /home/.z/chat-uploads/DoubleSkill_v.0.1-9531356b0134.cab \
  --cpu unicorn \
  --max-slices 3000000 \
  --instructions-per-slice 1000000 \
  --message-budget 0 \
  --dump-frames-to proof/doubleskill-windows-mobile/tap-frames \
  --max-frames 12
```

Result: **PASS** — exit status 0, clean emulator exit, `frame_counter=24`, 12 captured frames, and no unimplemented API-call warnings. The exact helper output is in `ai-tap-sequence.log`.

`contact-sheet.png` shows the startup-to-render sequence, including the game interface and control graphics. `first-render.png` is the first non-black render frame. The PPM files in `tap-frames/` and `startup.ppm` are lossless framebuffer evidence.

The screenshots are PocketHLE emulator captures using the ARM Unicorn backend, not screenshots from a physical Windows Mobile device; no native Windows Mobile device emulator was available in the Linux host environment.
