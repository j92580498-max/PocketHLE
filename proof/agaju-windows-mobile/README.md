# Agaju — The Sacred Path of Treasure (Windows Mobile) proof

The supplied Agaju prototype was run through PocketHLE's ARM Unicorn backend with `tools/ai-tap-sequence.py`.

## Result

- Automated tap sequence: **PASS**
- Taps: `(120,200)`, `(120,220)`
- Screen: **320×240** landscape, matching the prototype's Windows Mobile GLES layout
- Captured frames: **8**
- Final `frame_counter`: **8**
- Emulator exit: clean, status 0
- Final framebuffer: non-black rendered Agaju menu with logo, background artwork, menu entries, and play control

## Fix

Agaju places its initial menu geometry exactly on the OpenGL ES near clipping plane (`z + w == 0`). PocketHLE's rasterizer used a positive epsilon when deciding whether a vertex was inside that plane, so the entire menu was discarded even though the game initialized EGL, uploaded textures, drew geometry, and called `eglSwapBuffers`. The fix keeps geometry exactly on the valid clipping boundary and adds a regression test.

The screenshots are emulator-side proof; no native Windows Mobile device or Windows Mobile SDK emulator was available in this environment.
