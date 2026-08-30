# Rayman Ultimate (Windows Mobile) proof

Supplied game: `RaymanUltimate` folder (`RaymanUltimateARM.exe`, ARM Pocket PC,
with `GX.dll`, `alib.dll`, `zlib.dll` and gzipped `PCMAP/` levels).

## Symptom

The process ran and called `GXOpenDisplay` (framebuffer mapped at
`0x78000000`), but `frame_counter` stayed `0`: the trace was an endless
`GetMessageW` → `DispatchMessageW(WM_PAINT)` loop on the primary thread.

## Root cause

The game parks its engine on a worker thread (`CreateThread`, entry
`0x8f25c` → engine body `0x7bba8`) and leaves only the classic
`GetMessageW`/`DispatchMessageW` pump on the primary thread. The
synthetic message pump fabricates `WM_PAINT` traffic, so the primary's
blocking `GetMessageW` succeeded forever — and because a *satisfied*
`GetMessageW` never reached the scheduler's resumption checks, the
parked engine worker never ran again. The game was starved by
construction: no engine ticks, no rendering, `frame_counter = 0`.

## The fix

`crates/pocket-winceapi/src/coredll.rs`:

- `get_message_w` (blocking, primary-thread path) now calls
  `resume_worker_reenter` *before* fabricating pump traffic;
- `resume_worker_reenter` resumes one parked, started worker exactly
  once, marks the primary as `parked_in_pump`, and re-enters the pump
  (`JumpTo(ctx.thunk.thunk_va)`) when the worker yields again, so the
  primary's `GetMessageW` call site is preserved across the hand-off;
- `GuestThread::parked_in_pump` (`crates/pocket-kernel/src/lib.rs`)
  keeps that state out of the park helpers' restore paths.

Because a dispatched message can itself park or retire threads, worker
eligibility is re-evaluated after every dispatched message, and
`quit_or_halt` still owns exhaustion (`parked_in_pump` is cleared on
both paths so the pump keeps its own shutdown invariants).

## Supporting change: `alib.dll` HLE

Rayman Ultimate imports `gzopen` / `gzseek` / `gzread` / `gzclose` from
`alib.dll`, a small zlib wrapper shipped beside the executable, to load
its gzipped map data (`PCMAP/**/*.lev.gz`, `allfix.dat.gz`). The
emulator had no `gz*` handlers, so every load failed.

`crates/pocket-kernel/src/gz.rs` adds a host-side gzip file table:
gzip member parsing, `flate2` inflate, guest path resolution through
the existing rom-dir mount, and the four entry points as ordinal-style
handlers in `crates/pocket-winceapi/src/game_dlls.rs`
(`register_alib`). `gzseek` supports `SEEK_SET`/`SEEK_CUR`/`SEEK_END`,
and `gzread` copies decompressed bytes into guest memory.

## Verification

Build:

```text
cargo build --release -p pocket-cli --features unicorn
cargo test --workspace
```

`pocket-kernel` (91 tests) and `pocket-winceapi` (99 tests) pass,
including the new gzip-table tests and the CreateThread parking
regression test. `cargo fmt --all -- --check` is clean.

Run (ARM Unicorn backend):

```text
pockethle run RaymanUltimateARM.exe \
  --rom-dir RaymanUltimate \
  --module-path '\Application\RaymanUltimateARM.exe' \
  --cpu unicorn --message-budget 0 --dump-frames-to frames --max-frames 60
```

`frame_counter=60`, clean exit. The captures show the boot sequence
rendering on the 240×320 GAPI framebuffer: black startup, the fading-in
orange Gameloft logo, and the logo at full brightness in the final
frames (content brightness grows monotonically: mean luminance
0.0 → 2.5 → 7.6 → 8.2, peak 149).

The requested tap helper also passes:

```text
python3 tools/ai-tap-sequence.py RaymanUltimateARM.exe \
  --pockethle target/release/pockethle \
  --max-frames 15 --max-slices 100000 --message-budget 0 \
  --dump-frames-to frames
```

Exit code 0, clean exit, `frame_counter=217`
(`ai-tap-sequence.log`).

## Known follow-up

The engine's gz* loads now resolve through the mount, but the supplied
folder keeps the `RaymanUltimate` sub-directory in its paths; if a
future trace shows map lookups failing at a deeper level, the next step
is matching the guest's working directory against the mount prefix in
`vfs.rs` — out of scope here, where the goal was reaching the render
path, which the logo frames demonstrate.