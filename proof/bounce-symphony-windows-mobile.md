# Bounce Symphony — Windows Mobile proof

The supplied ARM executable was run with the required `tools/ai-tap-sequence.py` harness. The baseline stopped in the startup/message-pump path with `frame_counter=0`; after the `gd201b.dll` GAPI compatibility and worker scheduling fixes, the run exited successfully with `frame_counter=8` and produced two captured framebuffer frames.

- Harness output: `bounce-symphony-windows-mobile-ai-tap.log`
- API trace: `bounce-symphony-windows-mobile-api-trace.jsonl`
- Frame 0: `bounce-symphony-windows-mobile-frame-000000.png`
- Frame 1: `bounce-symphony-windows-mobile-frame-000001.png`

The container has no Windows Mobile device/emulator, so these are PocketHLE's deterministic Unicorn ARM framebuffer captures, not a physical-device screenshot.
