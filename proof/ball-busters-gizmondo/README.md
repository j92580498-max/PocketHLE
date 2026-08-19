# Ball Busters (Gizmondo, EN beta) proof

This proof uses the supplied `Ball-Busters_Gizmondo_EN_Beta.zip` card image —
an ARM Windows CE build of NET DOL / Fathammer's *Ball Busters* on a
`GZGA200045` Gizmondo card.

On the clean `main` build the game froze on the SHAM publisher logo around
frame 230 and spent the rest of the run in its message pump
(`publisher-logo-freeze-before.png`). With that cleared it got as far as
complaining that the storage card had been removed, because the card's
serial number read back as zero.

- Test tool: `pockethle run` (`pocket-cli`, `--features unicorn`)
- CPU: Unicorn ARM
- Input: `Enter` (`VK_RETURN`) presses at frames 900, 1000, 1900, 2050, 2200, 2350, 2500, 2650 and 2800
- Result: clean exit, 3000 captured frames, final `frame_counter=3000`
- Frame diversity: 1154 unique framebuffer images
- Visible result: SHAM publisher logo, the legal card, the Fathammer logo, the starfield intro, the "ball BUSTERS ™ ©2005 NET DOL CO., LTD. — PRESS PLAY BUTTON" title, the Menu (Arcade / Quest / Options / Exit Game), character and racket select, the opponent introduction, and an Arcade match: the court in perspective, the aiming reticle, the incoming ball, and the timer / rally / stage HUD
- Runtime warnings/errors: none

## What the fix covers

- **The storage card is present and has a serial.** `\SD Card\Vol:` opens as
  a handle on the volume rather than a file, and
  `IOCTL_DISK_GET_STORAGEID` fills a real `STORAGE_IDENTIFICATION`: the
  first call fails with the required size in `dwSize`, the second fills the
  header plus the manufacturer and serial strings. The game reads the serial
  with `strtoul(id + id->dwSerialNumOffset, NULL, 10)`, so a zeroed buffer
  gave it serial 0 and its "storage card removed" screen. The serial itself
  comes from the card: beside the game directory sits a four-byte marker
  file with the same name as that directory
  (`\SD Card\GZGA200045\GZGA200045`), holding the serial of the card the
  title was published on. `storage-card-ioctl.log` is that exchange.
- **`MAS1:`, the Gizmondo's Micronas MP3 decoder, opens and accepts
  frames.** Nothing here decodes MP3, but the game builds its music player
  by opening the device first and the file second, and on failure leaves the
  player zeroed — then calls it on the next loading tick and dereferences a
  NULL stream.
- **An ignored `WM_QUIT` halts the run instead of repeating.** X-Forge only
  acts on a quit that arrived through `GetMessage`; the one its
  `PeekMessage` pump gets is dispatched like any other message and dropped.
  Answering it forever is what pinned the game to the publisher logo.
- **Screen geometry.** The Gizmondo's LCD is 320x240 landscape and the game
  lays its menus out from the display size it reads at start-up, so
  recognising the card layout now sets that geometry before the run.

## Verification

- `cargo build --release -p pocket-cli --features unicorn` — passed.
- `cargo test --workspace` — passed.
- Run: `pockethle run Ball-Busters_Gizmondo_EN_Beta.zip --cpu unicorn
  --message-budget 0 --dump-frames-to <dir> --max-frames 3000
  --key 900:enter --key 1000:enter --key 1900:enter --key 2050:enter
  --key 2200:enter --key 2350:enter --key 2500:enter --key 2650:enter
  --key 2800:enter` — see `run.log`.
- No `MessageBoxW` call appears in the run: a debug-level 600-frame run,
  which covers the whole start-up card check, logs zero of them. The
  storage-card path is not reached at all, rather than reached and
  dismissed.

`character-select.png` and `frame_001980.ppm` cover the menus,
`arcade-match.png` / `frame_002640.ppm` an Arcade rally. The screenshots are
emulator-side proof, not screenshots from a physical Gizmondo device.
