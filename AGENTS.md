# PocketHLE — start here

PocketHLE runs Windows CE / Windows Mobile applications by **high-level
emulation**: guest ARM (or MIPS) code is executed instruction-by-instruction,
but every call into a Windows CE DLL is intercepted at the import boundary and
serviced by clean-room Rust. There is no emulated `coredll.dll`, no emulated
kernel, no emulated drivers.

**The full architecture reference is [`docs/AGENTS.md`](docs/AGENTS.md). Read it
before changing anything.** It is the single source of truth, kept deliberately
in one file so it cannot drift out of sync with itself. It documents the
invariants that are not recoverable from reading any single source file — the
ones a plausible-looking refactor breaks — plus the address space, the three IAT
strategies, the dispatch path, the two presentation paths, the MSVC / CeGCC
toolchain split, and the synthetic message pump.

Four things worth knowing before you even open it:

* Dispatcher handler keys are `("coredll.dll", "MessageBoxW")` — lower-cased,
  **always** with the `.dll` suffix. Four independent consumers depend on this.
* `frame_counter=0` after an otherwise clean run is a *presentation* problem,
  not a CPU one.
* A game that stops producing frames but exits *cleanly* is usually the CLI's
  240-message cap, not a bug. Re-run with `--message-budget 0` first.
* Comments in this codebase explain *why a specific game needs this*, naming the
  title. Match that.

```bash
cargo build --release -p pocket-cli --features unicorn   # → target/release/pockethle
cargo test --workspace
cargo clippy --workspace --all-targets
```

Do not commit unless asked, and never push directly to the target branch —
changes go through a Pull Request.
