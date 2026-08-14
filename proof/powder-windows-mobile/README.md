# POWDER Windows Mobile proof

This proof uses the public Windows Mobile POWDER 111 CAB (`powder-reference.CAB`, kept locally for testing and ignored by Git). The SDL 1.2 compatibility layer now covers the imported video, timer, event, surface, and input entry points.

## Verification

```sh
cargo build --release -p pocket-cli --features unicorn
cargo test --workspace
cargo clippy -p pocket-winceapi -p pocket-kernel --all-targets -- -D warnings
python3 tools/ai-tap-sequence.py /path/to/powder.CAB \
  --cpu unicorn --tap 120,160 --tap 120,220 \
  --message-budget 0 --max-frames 4 --frames /tmp/powder-ai-final \
  --pockethle target/release/pockethle
```

The tap sequence exits with code 0, reaches the SDL event loop, produces two changed framebuffer snapshots, and reports a clean emulator exit without unimplemented-call or CPU-crash errors. `gameplay.ppm` is the second changed 320x240 framebuffer snapshot.
