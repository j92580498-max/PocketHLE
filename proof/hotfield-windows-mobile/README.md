# Dungeon and Hero — Windows Mobile proof

The supplied `Dungeon_and_Hero_105-443960a4a66f.cab` was run with PocketHLE's ARM Unicorn backend.

## Acceptance run

```text
python3 tools/ai-tap-sequence.py Dungeon_and_Hero_105-443960a4a66f.cab \
  --pockethle target/release/pockethle \
  --cpu unicorn \
  --max-slices 5000000 \
  --instructions-per-slice 1000000 \
  --message-budget 240 \
  --tap 120,260 \
  --tap 80,180 \
  --dump-frames-to proof/hotfield-windows-mobile/final-105-frames \
  --max-frames 60
```

Result: **PASS**. The rebuilt emulator exited cleanly, rendered 60 framebuffer snapshots at 240x320, and reached `frame_counter=216`.

The fix preserves `MK_LBUTTON` in `wParam` for `WM_LBUTTONUP`. Windows Mobile games that handle touchscreen release through the standard Win32 mouse-message path rely on this button-state bit.

`dungeon-and-hero-final.png` is the final captured framebuffer; `final-105-frames/` contains the complete raw capture.
