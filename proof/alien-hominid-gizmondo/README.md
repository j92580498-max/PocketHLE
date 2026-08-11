# Alien Hominid (Gizmondo) proof

This proof uses the supplied Alien Hominid Gizmondo ZIP. The title is a Windows Mobile / Windows CE ARM Thumb application built for Gizmondo.

- Test tool: `tools/ai-tap-sequence.py`
- CPU: Unicorn ARM
- Input: scheduled `Enter` presses at frames 1, 20, 40, 60, 80, 100, 120, 140, 160, 180, 220, 300 and 400
- Result: clean exit, 200 captured frames, final `frame_counter=200`
- Frame diversity: 75 unique framebuffer images
- Visible result: Gizmondo splash, Newgrounds splash, Alien Hominid title screen, then rendered gameplay with the character and city scene
- Runtime warnings/errors: none

The fix covers the Gizmondo-specific path and rendering requirements: WinCE-compatible PE parsing for the certificate record, `\\SD Card\\` mounting and module-path reporting, DXT1 textures, OES paletted textures, and the imported integer `abs` handler.

The contact sheet covers the Gizmondo splash, Newgrounds/Behemoth/Technologie title cards, menu, and street gameplay. The screenshots are emulator-side proof, not screenshots from a physical Gizmondo device.
