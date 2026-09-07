# vAlienAttack (Windows Mobile) — compatibility report

Source: `SetupvAlienAttack-spaces.im-2ec7b3cd4177.cab`

## Result

The game now launches and reaches gameplay under PocketHLE's managed
runtime, including the tap sequence required by
`tools/ai-tap-sequence.py`. Screenshot evidence:

- `menu.png` — the game window on launch: soft-key menu (`Start` /
  `Exit`) and the game's backdrop state before starting.
- `gameplay.png` — after tapping `Start` and a tap in the play field:
  the alien formation (five coloured rows), the starfield, the player
  defender and a missile in flight are all rendered.

## Root cause of the launch failure

`vAlienAttack.exe` is not a native ARM game: it is a managed x86 image
(CLR runtime v2.0.50727) written against the .NET **Compact** Framework.
It references `Microsoft.WindowsCE.Forms, Version=3.5.0.0` for
`SystemSettings.ScreenOrientation`, an assembly that does not exist on
the host Mono runtime. The process died with an unhandled
`TypeLoadException` before any window opened, so `--dump-frames-to`
produced no frames at all — the observed "frame_counter stays 0".

## Fix

1. **Compact-Framework compatibility shim**
   (`frontends/pocket-cli/managed-compat/`) — a tiny assembly that
   provides `Microsoft.WindowsCE.Forms.SystemSettings` (with
   `ScreenOrientation`) mapped onto desktop equivalents. It is built
   with `mcs` and committed so no .NET build step is needed; the managed
   runner now prepends that directory to `MONO_PATH`.
2. **Virtual display** (`frontends/pocket-cli/src/xvfb.rs`) — when no
   `DISPLAY` is available, the managed runner spawns a temporary Xvfb at
   the emulated device size (e.g. `240x320`, overridable with
   `--screen`) instead of requiring a host GUI, and cleans it up after
   the run.
3. **Menu tap** (`managed.rs`) — xdotool click → press, short hold,
   release, because Mono's WinForms menu only opens the dropdown while
   the button is held.

## Test

```
python3 tools/ai-tap-sequence.py \
  /home/.z/chat-uploads/SetupvAlienAttack-spaces.im-2ec7b3cd4177.cab \
  --pockethle target/release/pockethle --cpu unicorn \
  --message-budget 0 --max-slices 100000 --max-frames 5 \
  --screen 240x320 --tap 12,8 --tap 120,200 \
  --dump-frames-to proof/valien-attack-windows-mobile/tap-frames
```

The full log is in `ai-tap-sequence.log`; it ends with the frame dump
`managed-000000.png` shown as `gameplay.png`.

## Known cosmetic difference

Mono's WinForms renders the `MainMenu` as a menu strip at the top of the
window. On Windows Mobile the same `MainMenu` is drawn as the bottom
soft-key bar (with `Start` on the left and `Capt…` on the right, next to
the hardware-key icons). The menu items, taps and gameplay are
identical; only the position of the bar differs on the host runtime.

## Sprite rendering fix (white halos)

The first gameplay capture showed every sprite surrounded by opaque white
pixels — the sprite bitmaps' black background is made transparent by
`ImageAttributes.SetColorKey`, and Mono's libgdiplus dropped the alpha when
the clone it processes carries no alpha channel (24bppRgb PNG sprites),
leaving keyed pixels as opaque white. Fixed in libgdiplus itself; see
`frontends/pocket-cli/managed-compat/README.md` ("Sprite color-key
transparency") for the patch and build instructions. `gameplay.png` was
captured with the patched libgdiplus and matches the original device
rendering.
