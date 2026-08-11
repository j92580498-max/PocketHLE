# The Quest — Windows Mobile proof

The supplied `TheQuestPpc103-99e72fe44d3f.cab` is an ARM Pocket PC CAB for Redshift's *The Quest*. On the clean `main` build it stopped during startup at a null guest call after dynamically resolving the GAPI surface. The missing export was `?GXSetViewport@@YAHKKKK@Z`; the game treats a null function pointer as a fatal startup condition.

The fix registers `GXSetViewport` in `gx.dll`, returns success, and records the four viewport arguments without changing the framebuffer. The existing Windows Mobile fixes on this branch also preserve `SetWindowLongW`'s previous value, correctly return from guest window callbacks, and model semaphore state for worker synchronization.

## Verification

- `cargo build --release -p pocket-cli --features unicorn` — passed.
- `cargo test --workspace` — passed.
- Fixed run: `pockethle run TheQuestPpc103-99e72fe44d3f.cab --cpu unicorn --max-slices 12000000 --instructions-per-slice 1000000 --message-budget 0 --dump-frames-to ... --max-frames 20` — exit 0, clean emulator exit, `frame_counter=21`, 20 distinct captured frames.
- Required tap tool: `python3 tools/ai-tap-sequence.py TheQuestPpc103-99e72fe44d3f.cab --message-budget 0 --max-slices 3000000 --max-frames 12 --tap 80,100 --tap 160,100 --dump-frames-to ...` — exit 0, 2 captured frames, no emulator crash.
- Captured frame diversity: the longer run produced 20 unique PPM snapshots; `gameplay.ppm` contains non-black rendered artwork.

The test host has no ALSA output device, so the run logs the expected audio-backend warning. It does not affect startup or framebuffer rendering.
