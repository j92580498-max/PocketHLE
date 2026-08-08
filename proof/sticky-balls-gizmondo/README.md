# Sticky Balls (Gizmondo / Windows Mobile) proof

This proof uses the uploaded `Sticky-Balls_Gizmondo_EN-2fc03762fd9b.zip` archive and the ARM Unicorn runner.

- Command: `python3 tools/ai-tap-sequence.py <archive> --cpu unicorn --tap 120,160 --tap 120,220 --max-frames 80 --max-slices 10000000 --instructions-per-slice 1000000 --message-budget 1200`
- Result: clean exit, 80 framebuffer snapshots, final `frame_counter=129`.
- The first frame is the expected black initialization frame; later captures contain rendered GLES content.
- The run emits no `eglSwapIntervalNV` warning after adding the NVIDIA/Gizmondo EGL vendor exports.
- The full workspace test suite passes.

`gameplay.png` is an enlarged capture of `frame_000079.ppm`.
