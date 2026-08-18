# Zuma (Pocket PC) proof — `Sleep` delivers `waveOut` buffer completions

Title: `Zuma_v1.50.cab` (`ZumaPPC_VS2008.exe`, Astraware/PopCap, 2005),
software renderer, no OpenGL. CPU: Unicorn ARM. Host: AMD Ryzen 7 7730U,
4 cores.

## What broke

Zuma's sound engine stops a stream by setting a request byte and then
spinning:

```text
0x000e80e8  strb #1, [r4, #160]      ; stop_request = 1
0x000e8118  mov  r0, #0
0x000e8120  bl   Sleep               ; Sleep(0)
            ldrb r3, [r4, #161]      ; stop_ack
            cmp  r3, #0
            beq  0x000e8118
```

The only code in the image that ever writes `stop_ack` is the game's own
`waveOutProc` at `0x000e896c`, which the driver calls with `WOM_DONE`.
On WinCE the audio driver's thread makes that call, so the acknowledgement
arrives while the game sleeps. PocketHLE only serviced the wave queue from
`waveOutWrite` and from the message pump, and a `Sleep(0)` spin reaches
neither — so the loop never ended. The game burned a full core forever
after nine to eighty rendered frames, which looks like catastrophic
performance rather than a hang.

`Sleep` is now the third `service_wave_out` caller. Because Zuma's
`waveOutProc` itself calls `Sleep(0)` before acknowledging, the callback
detour is unwound only when SP is back at its saved value less the
reserved stack argument (`WAVE_PROC_STACK_BYTES`) — the same SP test
`create_window_ex_w` uses. Regression test:
`sleep_delivers_wave_out_callback_and_survives_a_nested_sleep`.

## Before / after, same command

`--message-budget 240` posts a synthetic `WM_QUIT`, which is what makes the
game run its shutdown handshake:

| | wall | frames | `Sleep` calls | outcome |
|---|---|---|---|---|
| before | 34.3 s | 9 | 49,229,444 | hit the 50 M-slice cap, still spinning |
| after | 1.4 s | 9 | 227,466 | clean exit, `waveOutClose` reached |

## Sustained runs

800x480, in-game (`LVL 1-1`), 4,000 frames:

```text
wall 124.728s  slices 5817222  frames 4000  32.1 fps
mean 30.9 ms (32.3 fps)  median 30.0 ms  p90 31.9 ms  p99 60.3 ms
71 frames over 40 ms (1.8%)
```

240x320, in-game, 4,000 frames: 30.4 fps, median 30.1 ms, p90 31.7 ms,
p99 61.1 ms, 271 frames over 40 ms (6.9%).

800x480 menu/attract screen, 7,982 frames over 244.8 s: 32.6 fps, median
30.0 ms, p90 34.2 ms, p99 41.7 ms, all 400 dumped frames distinct — the
screen never stops animating, which is the part the wedge used to kill.

The ~30 ms floor is the game's own pacing, not ours: 64.7% of host wall
time is spent idle inside `GetMessageW` waiting for the guest's next timer
deadline, and guest CPU is 30.8%.

Audio is continuous, not just started: a 46.4 s run recorded 43.9 s of
22.05 kHz mono PCM from 1,958 `waveOutWrite` calls, every one of them
retired.

## Screenshots

`contact-sheet.png` is the whole captured run — every 60th rendered frame
of a 1,382-frame 800x480 session: blank window, Astraware splash with its
progress bar, PopCap splash, the Adventure level-select screen (Temple of
Zuxuxan), two instruction cards, then eighteen frames of `LVL 1-1` with the
ball chain advancing. The individual shots are `boot-astraware-splash.png`,
`level-select.png`, and `gameplay-{start,mid,late}.png`. They are
emulator-side captures, not photographs of a device.

## Reproducing

```sh
cargo build --release -p pocket-cli --features unicorn
KEYS="--key 100:enter --key 160:enter --key 220:enter --key 300:enter --key 400:enter"
POCKETHLE_PROFILE=1 ./target/release/pockethle run Games/Zuma_v1.50.cab \
    --cpu unicorn --screen 800x480 --message-budget 0 --max-slices 0 \
    --max-frames 4000 $KEYS
```

The `--key enter` presses walk the first-run name dialog; without them the
run stays on that dialog and never reaches a level. Drop `--max-slices 0`
and use `--message-budget 240` to reproduce the old wedge against a
build without the fix.
