# Bubble Trouble — Windows Mobile proof

## Fix

Bubble Trouble imports `aygshell.dll` ordinal 75 (`SHLoadImageFile`) for its external BMP/JPEG graphics. PocketHLE previously treated the ordinal as an unimplemented call and stopped before rendering. The fix registers the ordinal, resolves the guest path through the VFS, decodes the image, and returns a GDI bitmap in the emulator's RGB565 format.

## Acceptance run

```text
python3 tools/ai-tap-sequence.py /home/.z/chat-uploads/Bubble_Trouble_1.0b-60bed77e9a06.cab \
  --pockethle target/release/pockethle \
  --cpu unicorn \
  --max-slices 3000000 \
  --instructions-per-slice 1000000 \
  --message-budget 0 \
  --tap 120,260 \
  --tap 120,220 \
  --dump-frames-to proof/bubble-trouble-windows-mobile/gameplay-frames \
  --max-frames 12
```

Result: **PASS**. The command exited with code 0, the emulator exited cleanly, and the final framebuffer had `frame_counter=1`. The captured screen is 240×320 and displays the Bubble Trouble menu artwork and controls; the raw framebuffer is `gameplay-frames/frame_000000.ppm`, with a PNG preview in `gameplay.png`.

The full workspace test suite also passed: **365 passed, 10 ignored, 0 failed**. Audio warnings are expected on this headless Linux host because no default ALSA device is available.

The screenshot is a PocketHLE emulator capture using the ARM Unicorn backend, not a screenshot from a physical Windows Mobile device; no native Windows Mobile device emulator was available in the Linux host environment.
