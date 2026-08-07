# WinCE guest-loop rendering proof

This capture verifies the runtime watchdog fix for a Windows CE guest that can remain inside an API-free loop between host dispatch points.

- The CPU backend now uses a 16 ms per-slice wall-clock watchdog by default.
- The CLI, desktop, and Android runners explicitly request the same 16 ms slice timeout.
- The framebuffer capture reached a rendered gameplay screen instead of remaining at `frame_counter=0`.
- `tomatl-gameplay.png` is the checked-in visual proof from the emulator-side run.

The proof is produced by PocketHLE's Unicorn ARM backend. It is not a native Windows Mobile device screenshot.
