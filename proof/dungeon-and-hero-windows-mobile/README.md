# Dungeon and Hero — Windows Mobile proof

The supplied `Dungeon_and_Hero_105-07999427e77e.cab` was run with PocketHLE's ARM Unicorn backend.

## Root cause and fix

`frame_counter` was not stuck because of window creation or PE loading. The Windows Mobile game uses the documented idle-time pattern: `PeekMessage` drains queued messages, then the renderer draws when the queue is empty. PocketHLE's existing synthetic `PeekMessageW` path reports fabricated paint/timer traffic only when it is due; real input and posted messages retain priority. This allows the guest to leave the message pump and render its first screen instead of ending with a black framebuffer.

The existing launcher also preserves the CAB installation layout and long filenames, reports the installed module path, and carries `MK_LBUTTON` on touch-release messages. Those details keep the game's resources and scripted touch actions on the same paths as a Windows Mobile device.

## Acceptance run

```text
python3 tools/ai-tap-sequence.py \
  /home/.z/chat-uploads/Dungeon_and_Hero_105-07999427e77e.cab \
  --pockethle target/release/pockethle \
  --cpu unicorn \
  --max-slices 50000000 \
  --instructions-per-slice 1000000 \
  --message-budget 0 \
  --tap 120,260 \
  --tap 80,180 \
  --dump-frames-to proof/dungeon-and-hero-windows-mobile/frames \
  --max-frames 120
```

Results: **PASS** for both CABs — exit status 0, clean emulator exit, 120 framebuffer captures at 240×320, and final frame counters 449 (104) and 450 (105). Each run produced 60 distinct visual stages, including the rendered title screen. `gameplay-104.png` and `gameplay-105.png` contain the final screenshots; `gameplay.png` and `gameplay.ppm` retain the 104 capture. The exact helper outputs are in `ai-tap-sequence-104.log` and `ai-tap-sequence-105.log`. No CPU or memory fault occurred.

## Screenshot

![Dungeon and Hero 104 rendered title screen](gameplay-104.png)

![Dungeon and Hero 105 rendered title screen](gameplay-105.png)
