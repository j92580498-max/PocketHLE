# Bubble Bash 2 — Windows Mobile compatibility report

## Result

The supplied `WM_BubbleBash2_Samsung_SGHI617_EN_IGP_Mi-spaces.im-59e77bb52515.cab` boots through PocketHLE's Windows Mobile path and reaches the game's native display initialization. The tested payload is the ARM Thumb executable `Bubble Bash 2.exe` from Gameloft.

The launch failure was caused by a lifetime bug in `CreateDIBSection`: the emulator allocated the guest-visible DIB pixel buffer from the process heap, but `DeleteObject` removed the GDI bitmap without returning that buffer to the heap. Bubble Bash creates and destroys DIB-backed resources during its startup/render path, so repeated cycles consumed the fixed guest heap and left the display blank. `DeleteObject` now removes the bitmap while retaining its metadata long enough to free `dib_bits_va`; stock GDI objects remain protected.

The fixed run reaches `GXOpenDisplay()` and stays in the native message loop without a CPU fault. The captured framebuffer is currently a blank white 320×240 surface; therefore this PR claims the launch/display-initialization fix, not a visually verified gameplay screen. A remaining rendering issue is that Bubble Bash's first visible scene is not yet reaching the presented framebuffer.

## Verification

The required helper was run with the supplied CAB and the fixed release binary:

```text
python3 tools/ai-tap-sequence.py \
  /home/.z/chat-uploads/WM_BubbleBash2_Samsung_SGHI617_EN_IGP_Mi-spaces.im-59e77bb52515.cab \
  --pockethle target/release/pockethle \
  --cpu unicorn \
  --screen 240x320 \
  --max-slices 100000 \
  --instructions-per-slice 1000 \
  --message-budget 0 \
  --tap 120,160 \
  --tap 120,220 \
  --dump-frames-to proof/bubble-bash-windows-mobile/frames \
  --max-frames 8
```

Result: exit status 0, clean emulator exit, `frame_counter=18`, `GXOpenDisplay() -> 1`, and one captured framebuffer snapshot. The run reached the native game display setup and processed the synthetic tap sequence without a runtime error.

The baseline run also exited cleanly but stopped at `frame_counter=17`, produced eight all-white captures, and never logged `GXOpenDisplay()`. The fixed run reaches `GXOpenDisplay()` and preserves the heap backing of DIB sections across deletion. The host has no audio device, so the expected ALSA warning is present and audio is disabled; it does not affect video or input validation.

A fresh one-frame diagnostic run confirms `CreateDIBSection(320x240, 16bpp, top-up)` succeeds and `BitBlt` is called, but its captured frame remains all white. The visual gameplay criterion is not met yet.

## Tests

- `cargo test -p pocket-winceapi --lib -- --nocapture`: PASS — 98 passed, 0 failed.
- `cargo test -p pocket-kernel --lib -- --nocapture`: PASS — 89 passed, 0 failed.
- `cargo test --workspace --exclude pocket-desktop --exclude pocket-android-jni --no-fail-fast`: PASS.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: PASS.
- `cargo fmt --all -- --check`: PASS.
- `git diff --check`: PASS.

The regression test `create_dib_section_backing_memory_is_reclaimed_by_delete_object` directly verifies that a `CreateDIBSection` allocation is returned to the guest heap when the bitmap is deleted.

## Evidence

- `ai-tap-sequence.log` — required helper output for the fixed run.
- `baseline-ai-tap-sequence.log` — baseline comparison.
- `frames/frame_000000.ppm` — captured fixed-run framebuffer (blank white; retained as truthful evidence).
- `ai-tap-sequence-debug.log` — fresh diagnostic showing successful DIB creation, BitBlt, and clean exit.
