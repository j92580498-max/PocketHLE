# FreeMine / MineSweeper rendering proof

This proof uses the supplied `MineSweeper.ppc30_arm.CAB` from FreeMine 1.01.
The CAB contains a legacy ARM WinCE executable and Windows Mobile resources.

## Reproduction

```sh
cargo run -p pocket-cli --features unicorn -- \
  run /path/to/MineSweeper.ppc30_arm.CAB \
  --cpu unicorn \
  --max-slices 5000000 \
  --message-budget 1200 \
  --dump-frames-to /tmp/freemine-frames \
  --max-frames 100 \
  --dump-frame-stride 1
```

## Result

The run completed cleanly. The final emulator report was:

```text
Final framebuffer snapshot written to /tmp/pockethle-final.ppm (230415 bytes, frame_counter=199)
Emulator exited cleanly.
```

The checked-in `freemine-gameplay.png` is a 240x320 gameplay capture showing the minefield, counters, smiley button and all visible cells. The trace contains 99 `BitBlt` calls and multiple `GetUpdateRect`/`BeginPaint` calls during the run.

This is an emulator-side rendering proof using the real ARM guest code. It is not a screenshot from a physical Windows Mobile handset; this environment does not provide a Windows Mobile device emulator.
