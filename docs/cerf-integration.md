# CERF-inspired integrations

PocketHLE and [CERF](https://github.com/gweslab/cerf) solve different problems:
CERF emulates complete Windows CE devices, while PocketHLE runs individual
native game binaries through a high-level API boundary. CERF is MIT-licensed;
this document records which ideas are useful here and how they map onto
PocketHLE instead of copying CERF's device-specific C++ implementation.

## Adopted now

### Safe, layered virtual storage

The VFS now selects the most specific guest mount. This makes a broad ROM or
application mount safe to combine with a narrower save-data mount. Guest paths
remain case-insensitive, `..` cannot escape a mount root, and read-only mounts
are available for ROM and bundled assets.

The intended layout is:

```text
\\ROM\\            read-only extracted game assets
\\Application\\    writable per-game data and saves
```

A frontend can mount these independently with `Emulator::mount_dir` and the
kernel VFS API. The guest never receives an arbitrary host path.

### Atomic launcher metadata

`library.json`, `config.json`, and per-game manifests are written to a
temporary file and renamed into place. A cancelled or interrupted write should
not leave a half-written JSON manifest that prevents the launcher from opening.

### Documentation-driven boundaries

CERF's documentation distinguishes the machine layers clearly: CPU/JIT,
board and SoC peripherals, storage, presentation, input, and state. PocketHLE
uses the same separation at a smaller scale:

- `pocket-cpu` owns instruction execution and register/memory access.
- `pocket-kernel` owns process memory, VFS, framebuffer, and scheduling state.
- `pocket-winceapi` owns the host implementations of WinCE APIs.
- frontends own presentation and input translation.

This prevents board-level emulation from leaking into game-specific API
handlers.

## Deliberately not copied yet

- **Full-device board and peripheral emulation.** It is outside PocketHLE's
  HLE goal and would duplicate CERF's architecture rather than improve the
  current game path.
- **CERF save states.** CERF snapshots CPU, MMU, RAM, flash, peripherals, and
  presentation with a versioned section format and compatibility fingerprint.
  PocketHLE does not yet expose a complete serializable `Process`/CPU state,
  so pretending to support save states would be unsafe.
- **ROM container parsing.** CERF's ROM documentation covers NB0/B000FF,
  IMGFS, and OEM packages. PocketHLE currently imports CAB/ZIP/raw PE files;
  ROM-container support is a separate loader milestone, not a file-copy task.
- **Guest Additions, PCMCIA, serial modem, and network hardware.** These are
  complete-device features that do not belong in the current HLE core.

## Next implementation targets

1. Add a versioned, section-framed snapshot format after CPU and process state
   have explicit serialization APIs.
2. Add ROM-container inspection/extraction as a separate crate or CLI command,
   reusing the documented format rules without distributing Microsoft binaries.
3. Add deterministic crash reports containing the last guest PC, registers,
   active thunk, and a bounded memory window.

## References

- [CERF subsystem architecture](https://github.com/gweslab/cerf/blob/main/agent_docs/subsystems.md)
- [CERF feature guide](https://github.com/gweslab/cerf/blob/main/docs/website/content/articles/features.md)
- [CERF pointer/input guide](https://github.com/gweslab/cerf/blob/main/docs/website/content/articles/pointer-input.md)
- [CERF state image format](https://github.com/gweslab/cerf/blob/main/cerf/state/state_image_format.h)
- [CERF ROM container guide](https://github.com/gweslab/cerf/blob/main/docs/website/content/articles/rom-containers.md)
- [CERF command-line diagnostics](https://github.com/gweslab/cerf/blob/main/docs/website/content/articles/command-line.md)
