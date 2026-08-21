# Agaju — The Sacred Path of Treasure (Windows Mobile) proof

The supplied `Agaju-The-Sacred-Path-of-Treasure_Gizmondo_EN_Beta-73f3cecd4dd9.zip` was run through PocketHLE's ARM Unicorn Windows Mobile emulator using `tools/ai-tap-sequence.py`.

## Result

- Automated tap sequence: **PASS**
- Taps: `(120,200)`, `(120,220)`
- Emulated screen: **320×240** landscape
- Captured frames: **8**
- Final `frame_counter`: **8** (non-zero and incrementing)
- Emulator exit: clean, status 0
- Frames 0–4 cover startup/loading; frames 5–7 show the populated Agaju menu with logo, background artwork, menu entries, and play control.
- `startup.png` is the first populated menu frame; `gameplay.png` is the final captured menu frame.
- Full helper output is preserved in `ai-tap-sequence.log`.

## Fix

Agaju's initial menu geometry lies exactly on the OpenGL ES near clipping plane (`z + w == 0`). PocketHLE's software rasterizer used a positive epsilon (`1e-6`) when classifying vertices against that plane. The menu was therefore discarded even though the game initialized EGL, created a 320×240 RGB565 surface, uploaded its textures, issued draw calls, and called `eglSwapBuffers`.

The fix treats the exact clipping boundary as visible (`EPS = 0.0`) while retaining rejection for geometry behind the eye. A regression test covers a triangle on the near plane. The tap helper also accepts `--screen WIDTHxHEIGHT`, which is required to reproduce Agaju's landscape layout.

The proof is from PocketHLE's ARM Unicorn emulator. A native Windows Mobile device or Microsoft's legacy Device Emulator was not available in this environment.
