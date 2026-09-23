# Test results — Cops & Robbers / Windows Mobile

Verified 2026-09-23 with the supplied `QVGA/Cops_RobbersQVga.cab` on Linux using PocketHLE's ARM Unicorn backend.

## Regression and launch

- Legacy-cap reproduction (`--message-budget 240`): the sound prompt renders, then the game receives `WM_QUIT` and exits cleanly with `R0=0x00000042`; `frame_counter=1`.
- Default-budget run: the CLI resolves Cops & Robbers to `0` (unlimited); explicit `--message-budget` values still override this.
- `tools/ai-tap-sequence.py`: passed the startup tap and frame-scheduled menu/gameplay inputs. The run captured 48 changed frames at 240×320, reached active gameplay, and exited with status 0 at the requested capture limit (`frame_counter=2353`). The active gameplay snapshots are `../screenshots/gameplay.png` and `../screenshots/gameplay-later.png`.
- No unimplemented API calls were reported in the successful game run. The only environment warning was that this host has no ALSA output device.

## Project checks

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo build --release -p pocket-cli --features unicorn` | Passed |
| `cargo test --release -p pocket-cli --features unicorn` | 18 passed, 0 failed |
| `cargo test --workspace` | 391 passed, 10 ignored, 0 failed |
| `cargo clippy --workspace --all-targets` | Passed; no warnings |
| `python3 -m py_compile tools/ai-tap-sequence.py` | Passed |
| Tap helper dry-run, with omitted budget | Passed; no `--message-budget` flag emitted |
| Tap helper dry-run, explicit `--message-budget 240` | Passed; explicit override emitted |

The game itself was verified in PocketHLE; no physical Windows Mobile handset was available.
