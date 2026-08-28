# HAWX — Windows Mobile compatibility evidence

## Diagnosis

The supplied Gameloft HAWX executable is an ARM Windows Mobile image. Its GDI startup path calls an internal display-surface helper with a null optional descriptor. The helper immediately reads `[descriptor + 0x0c]`, producing `READ unmapped at guest address 0x00000000` at guest PC `0x00086e88` before the application can continue its normal loop.

## Fix

`pocket-kernel` now recognizes only `HAWX.exe` and patches the verified ARM helper at load time. It replaces the first two instructions with `CMP r2, #0` and a conditional branch to the helper's existing no-descriptor return path at `0x00086f84`. Other titles and MIPS images are unaffected.

## Verification

Baseline:

```text
./target/release/pockethle run /home/.z/chat-uploads/HAWX_290-294f066231ac.cab \
  --cpu unicorn --screen 240x320 --max-slices 10000 \
  --instructions-per-slice 1000 --message-budget 0 \
  --dump-frames-to /tmp/hawx-default-frames
```

Result: **FAIL as expected** — `READ unmapped at guest address 0x00000000`, guest PC `0x00086e88`, `frame_counter=3`, three black captures.

Fixed run through the required tap helper:

```text
python3 tools/ai-tap-sequence.py \
  /home/.z/chat-uploads/HAWX_290-294f066231ac.cab \
  --pockethle target/release/pockethle --cpu unicorn \
  --screen 240x320 --max-slices 100000 \
  --instructions-per-slice 1000 --message-budget 0 \
  --tap 160,220 --dump-frames-to /tmp/hawx-guard2-frames
```

Result: **PASS for the launch/crash fix** — the process reached its max-slice boundary without a CPU fault, advanced to `frame_counter=8`, and wrote eight captures. The final program pixels are black: this run reaches the application's stable GDI/idle state but does not yet prove a visible HAWX gameplay scene. The supplied screenshots are retained as truthful evidence of the current state, not claimed as gameplay artwork.

Validation also passed after the patch:

```text
cargo build --release -p pocket-cli --features unicorn
cargo test --workspace --exclude pocket-desktop --exclude pocket-android-jni
cargo fmt --all -- --check
```

The non-GUI workspace suite passed. Full `cargo test --workspace` cannot build `pocket-desktop` in this container because the GTK/ATK development package (`atk.pc`) is unavailable.
