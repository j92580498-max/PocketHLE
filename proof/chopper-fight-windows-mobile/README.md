# Chopper Fight 1.1 — Windows Mobile

## Fixes

The CAB's CE install header puts the executable in `bin/`, resources in `resources/`, and the manual in `manual/`. PocketHLE previously flattened that tree, missed sibling resource paths containing `..`, and didn't expand nested ZIPs in their installed directories. The game also relies on accurate class registration results, live GDI queries and text, a Windows CE display DC, and the correct button-release message state.

## Verification

- `tools/ai-tap-sequence.py` completed with exit code 0; emulator reported `Emulator exited cleanly` and `frame_counter=5565`.
- Captures show the title, room-selection, and team-selection UI. A combat round is **not** yet verified; the selection flow reports “Team is not ready.”
- `cargo fmt --all -- --check` — passed.
- `CARGO_BUILD_JOBS=1 cargo test -p pocket-cab -p pocket-kernel -p pocket-winceapi -p pocket-cli` — **118 passed, 0 failed**.
- `cargo test --workspace` was attempted, but the runner was killed with exit code 137 before completing; the affected crates passed in the serialized targeted run above.

## Screenshots

### Title

![Chopper Fight title screen](screenshots/title-screen.png)

### Team selection

![Chopper Fight team selection screen](screenshots/team-selection.png)

### Room selection

![Chopper Fight room selection screen](screenshots/room-selection.png)
