# Cybersaurus Windows Mobile proof

## Root cause

The supplied `Cybersaurus_trial.CAB` contains an ARM Pocket PC executable. The baseline reached `GXOpenDisplay`, produced one startup frame, then stopped at `max_slices=3,000,000` with the cooperative scheduler running the audio worker continuously. The worker's `WaitForSingleObject` waited on a semaphore returned by `CreateSemaphoreW`, but PocketHLE modeled that API as a constant fake handle and modeled `ReleaseSemaphore` as an unconditional success. The semaphore had no state, so the wait could not represent the device's synchronization point and the main thread never received CPU time for subsequent GAPI frames.

The fix adds guest-visible semaphore state to `KernelState`, implements `CreateSemaphoreW` and `ReleaseSemaphore`, consumes semaphore counts in `WaitForSingleObject`, and keeps the existing GAPI presentation path unchanged. This is why `frame_counter` now advances from `1` to `12+` and the rendered menu becomes visible.

## Verification

Build and tests:

```text
cargo build --release -p pocket-cli --no-default-features --features unicorn
cargo test --workspace --no-default-features --features pocket-cli/unicorn
cargo fmt --all -- --check
```

All workspace tests passed. The headless build was used because the test host has no display/audio device; the emulator's framebuffer capture is deterministic and is the same rendering path used by the desktop frontend.

The repository test helper was run against the supplied CAB:

```text
python3 tools/ai-tap-sequence.py \
  <extracted>/CBS_1.0_PPC_Trial/Cybersaurus_trial.CAB \
  --pockethle target/release/pockethle \
  --cpu unicorn \
  --max-slices 3000000 \
  --instructions-per-slice 1000000 \
  --message-budget 0 \
  --tap 120,160 --tap 120,220 \
  --dump-frames-to proof/cybersaurus-windows-mobile/tap-frames \
  --max-frames 12
```

Result: **PASS** — clean exit, `frame_counter=12`, 12 framebuffer captures, and no unimplemented-call or emulator-fault messages. The exact helper output is preserved in `ai-tap-sequence.log`.

## Screenshots

- `cybersaurus-gameplay.png` — rendered Cybersaurus main menu after the fix, 240×320.
- `cybersaurus-startup.ppm` — lossless first capture.
- `cybersaurus-gameplay.ppm` — lossless final capture from the verified run.

The checked-in screenshot shows the complete menu interface and artwork, including New game, Start game, Options, Credits, and Exit game.
