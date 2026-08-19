# ETK Micro Star Crusher — Windows Mobile investigation

- Payload: `ETK_Micro_Star_Crusher_v1.0-bf6860b793a2.exe` (ARM PE32 / Windows CE).
- Baseline: startup faulted at guest address `0x4fff2c00` while `frame_counter=0`.
- Fix implemented: mapped the legacy allocation-prefix window below `0x50000000`, separated 64 KiB-aligned `VirtualAlloc` reservations from the normal heap, exposed the region through `VirtualQuery`, and added dynamic `aygshell.dll` export/module handling.
- Verification: the emulator now exits cleanly and advances `frame_counter` to 170–194, producing 8 distinct 240x320 framebuffer captures.

## Current blocker

The supplied executable still opens its own `Error` dialog with the text **“Not Enough Memory to execute the game”** after the memory bootstrap. The captures therefore prove that the window, message pump, framebuffer, controls, and frame loop are alive, but they do **not** prove that the title reaches gameplay. The missing piece is the game's later startup/resource or platform check, not the original unmapped-memory crash.

## Automated test

The required harness completed with exit status `0` and clean emulator exit:

```text
python3 tools/ai-tap-sequence.py /home/.z/chat-uploads/ETK_Micro_Star_Crusher_v1.0-bf6860b793a2.exe --message-budget 0 --max-slices 500000 --instructions-per-slice 1000000 --dump-frames-to /tmp/star-crusher-final2 --max-frames 12
```

Observed: `frame_counter=194`, 8 framebuffer files of 230415 bytes each. A second run with `--tap 120,160 --tap 120,220` also exited with status `0` and produced 8 captures. The exact output is in `ai-tap-sequence.log`.
