# Arvale Windows Mobile proof

This proof covers the supplied Arvale Short Tales and Arvale II Ocean of Time CABs.

## Root cause

Both CABs store the complete game data as an inner ZIP payload (`0000data.001`, renamed by the install script to `data.zip`). PocketHLE materialized the long filename but never unpacked that payload, so the executable could open `data.zip` but every real sprite, font, INI, and screen asset lookup failed. The Short Tales CAB also used XML entities in its install paths (`&apos;`); those were being left literal, producing the wrong guest module path.

The fix:

- decodes XML entities in `_setup.xml` attributes;
- extracts nested `.zip` game payloads into the CAB mount directory;
- implements the imported `_strupr` CRT routine used by both Arvale executables.

The existing GAPI framebuffer and message-loop code then reaches the render path. `frame_counter` is non-zero in the final runs (`18`).

## Verification

Build:

```text
cargo build --release -p pocket-cli --features unicorn
cargo test --workspace
```

The workspace test suite passed. The ARM Unicorn runs exited cleanly and produced 240×320 PPM captures:

```text
pockethle run arvalest_sp-f3d22c995e65.cab --cpu unicorn --message-budget 240 --max-frames 40
pockethle run arvale2_ppc-0965c25411d4.cab --cpu unicorn --message-budget 240 --max-frames 40
```

The checked-in logs show successful nested-payload loading, GAPI display initialization, `frame_counter=18`, and clean exit. The captures include the GAPI diagnostic framebuffer (the four colored corner pixels) because this build stops in Arvale’s own display self-test before its normal title scene; they are proof that the framebuffer write/readback path is live, not a claim of full title-screen gameplay. `arvale-st-api-trace.jsonl` contains the API trace from the Short Tales run.

The requested tap helper was also run against Arvale II:

```text
python3 tools/ai-tap-sequence.py arvale2_ppc-0965c25411d4.cab \
  --cpu unicorn --tap 120,260 --tap 80,180 --max-frames 20
```

It exited cleanly with `frame_counter=18`; its concise output is in `ai-tap-sequence-final.log`.

## Captures

- `arvale-st-startup.jpg` — first captured frame
- `arvale-st-gameplay.jpg` — last non-black diagnostic frame from the run
- `arvale-st-frame.jpg` — final framebuffer snapshot
- `arvale-2-frame.jpg` — Arvale II final framebuffer snapshot
- `arvale-st-test-result.log` — concise Short Tales run output
- `ai-tap-sequence-final.log` — concise AI tap-sequence output
- `arvale-st-api-trace.jsonl` — API trace

The screenshots are provided as JPG files for convenient viewing. The original PPM captures are retained as lossless source evidence.
