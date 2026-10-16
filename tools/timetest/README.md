# TIMETEST v1 — ARM timing and callback diagnostic

Import `dist/PocketHLE-TIMETEST.zip` as a Gizmondo title. Run it once in Software
and once in GPU mode, restarting the title after changing the renderer. The
report is `\Flash Disk\TIMETEST.TXT`. Expect `TIMETEST_RESULT PASS` and no FAIL
lines. A final dialog must show PASS; close it normally.

Checks: repeated 20 ms Sleep, single/multiple wait timeouts and manual event
state, worker signaling/exit, timer callback delivery/cancel/restart, waveOut
EVENT/FUNCTION completion including a nested Sleep, 40 waveOut open/close
cycles, identical-frame EGL swap pacing and late-resume cadence, EGL teardown.
The PCM is silent, so the test measures completion rather than sound quality.
It is deliberately a contract diagnostic, not an exact hardware-speed benchmark.
Measured host latency is logged; long scheduling delays produce warnings rather
than fabricated success/failure thresholds. Early completion and incorrect API
results still fail. Worker/audio waits are bounded.

The guest clock has millisecond quantization; lower-bound tolerances are explicit.
Do not interpret a 60 Hz maximum as a requirement that every game run at 60 FPS.
Headless native regression uses software GLES and the virtual audio clock. Actual
CPAL/Android audio output and native GPU runs must also be tested on the device.
DirectDraw, pause/resume and visual/audio quality are covered by host contract
suites/manual games, not fully exercised by this guest program.

Build: `python3 tools/timetest/build.py --clang clang --lld ld.lld`.
Native integration: `cargo test -p pocket-core --no-default-features --features unicorn --test ram_guest native_arm_timing -- --nocapture`.
