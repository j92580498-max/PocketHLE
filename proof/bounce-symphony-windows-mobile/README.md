# Bounce Symphony — Windows Mobile proof

This proof uses the supplied Windows Mobile ARM executable `00Bounce.001`.

## Fix

Bounce Symphony imports `gd201b.dll`, a game-specific GAPI wrapper. Before the fix, its constructors, display setup, back-buffer acquisition, and timer calls were unimplemented; the guest then stayed in the message/wait loop and `frame_counter` remained `0`. The fix adds the required `gd201b.dll` compatibility handlers and makes the cooperative worker scheduler round-robin instead of repeatedly selecting the first worker.

## Test

```text
python3 tools/ai-tap-sequence.py 00Bounce.001 --pockethle target/release/pockethle --cpu unicorn --max-slices 120000 --instructions-per-slice 1000000 --message-budget 240 --dump-frames-to fixed4-frames --max-frames 8
```

The test exited with status `0`, reached `frame_counter=8`, and wrote eight framebuffer frames. The run was headless; the repository has no Windows Mobile emulator/device runtime, so the captures are PocketHLE's deterministic ARM/GAPI framebuffer proof rather than a physical-device screenshot.
