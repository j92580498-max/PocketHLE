# Bubble Breaker input regression proof

Tested the supplied ARM Windows Mobile CAB on PocketHLE's Unicorn backend at its VGA size (480×640). CAB SHA-256: `edb1d90703a5ab4f0c09d77871bcd758cd83c3738dc070cf12cde90bb64a15e2`.

## Root cause

The game installs a 1 ms timer with a non-null `TIMERPROC`. PocketHLE previously discarded the window and callback arguments, emitted `WM_TIMER` with `lParam=0`, and routed it to the WndProc. Windows CE instead puts the callback pointer in `WM_TIMER.lParam`; `DispatchMessageW` calls that callback with `(hwnd, WM_TIMER, idEvent, GetTickCount())`. Without it, the game accepted taps but did not perform the move or update the score.

The fix preserves the timer window/callback, places the callback address in `WM_TIMER.lParam`, and calls it through the guest callback trampoline. A null `TIMERPROC` still follows the ordinary WndProc path; `KillTimer` cancels the matching timer.

## Interaction test

`tools/ai-tap-sequence.py` ran with two taps at `(280,48)` on frames 1000 and 1100. The first run on the old dispatch path left the board at score 0. With the fix, the same sequence removes the selected group and changes the score from 0 to 6. The emulator exited cleanly at `frame_counter=3739`.

- [Before and after](interaction-before-after.png)
- [Gameplay after two taps](interaction-after-two-taps.png)
- [Tap-test result](interaction-ai-tap-sequence.log)

## Checks

- `cargo build --release -p pocket-cli --features unicorn` — passed.
- `cargo fmt --all -- --check` — passed.
- `cargo test --workspace --quiet` — 429 passed, 10 ignored, 0 failed.
- `cargo clippy --workspace --all-targets` — passed.

The host had no ALSA default audio device, so the run emitted ALSA device warnings and played silently; the game rendered and responded to the taps as shown.
