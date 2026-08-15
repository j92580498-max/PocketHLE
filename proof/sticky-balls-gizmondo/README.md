# Sticky Balls (Gizmondo) proof

This proof uses the supplied `Sticky-Balls_Gizmondo_EN.zip` card image — an
ARM Thumb Windows CE build of Gizmondo Studios' *Sticky Balls*, published on
a `GZGA200045`-style Gizmondo card.

On the clean `main` build the game ran but did not look right: the sky and
clouds came out as opaque white sheets, every ball had a hard square of
solid colour around it, and the whole scene was a portrait crop of a
landscape layout with the HUD off the edge of the screen.

- Test tool: `pockethle run` (`pocket-cli`, `--features unicorn`)
- CPU: Unicorn ARM
- Input: `Enter` (`VK_RETURN`) presses at frames 900, 1000, 1150, 1300, 1500, 1700 and 1900
- Result: clean exit, 2000 captured frames, final `frame_counter=4898`
- Frame diversity: 1998 unique framebuffer images
- Visible result: Gizmondo Games logo, the legal card, Gizmondo Studios Manchester, the language chooser, the Sticky Balls main menu, the Classic Game briefing, then Classic Game play — sky, clouds, the golden ramp, the ball tower and the water, with the score HUD across the top
- Runtime warnings/errors: none

## What the fix covers

Three separate causes, all visible in `contact-sheet.png`:

1. **Stage overflow.** X-Forge binds its colour map to texture stage 1 and
   then clears stage 2 before drawing. `glActiveTexture` past the last
   stage the context tracks is an error that leaves the selected unit
   *alone*, so that clear landed back on stage 1 and unbound the texture
   just bound there. An incomplete texture samples as opaque white, which
   is what turned the sky and clouds into white sheets.
   `stage-overflow-before-after.png` is the same frame index either side of
   the fix.
2. **Single-stage sampling.** A ball's colour is a
   `GL_COMPRESSED_RGB_S3TC_DXT1_EXT` map — a format with no alpha channel
   at all — on one stage, and its shape is the alpha of a white-RGB texture
   on the next. Sampling one stage only, the rasterizer had the colour but
   not the cut-out, so each ball kept an opaque square around it.
   `multitexture-cutout-before-after.png` is a 2× crop of the top of the
   ball tower either side of the fix: the square goes, and the golden glow
   ring behind it becomes visible.
3. **Screen geometry.** The Gizmondo's LCD is 320x240 landscape, and the
   game reads the display size once during start-up. Recognising the card
   layout now sets that geometry before the run, so `--screen` is no longer
   needed for a Gizmondo card.

## Verification

- `cargo build --release -p pocket-cli --features unicorn` — passed.
- `cargo test --workspace` — passed. The behaviours above are pinned by
  `resetting_a_stage_past_the_advertised_maximum_keeps_the_live_stage_bound`,
  `a_second_stage_alpha_mask_cuts_out_a_colour_map_that_has_no_alpha`,
  `a_later_stage_combines_with_the_colour_the_earlier_one_produced` and
  `a_gizmondo_card_comes_up_on_the_devices_landscape_screen`.
- Run: `pockethle run Sticky-Balls_Gizmondo_EN.zip --cpu unicorn
  --message-budget 0 --dump-frames-to <dir> --max-frames 2000
  --key 900:0x0d --key 1000:0x0d --key 1150:0x0d --key 1300:0x0d
  --key 1500:0x0d --key 1700:0x0d --key 1900:0x0d` — see `run.log`.
- Sky measurement over the 500 gameplay frames from 1500 on: the mean
  fraction of near-white pixels in the top 180 rows falls from 0.455 before
  the fix to 0.000 after it.

`main-menu.png` / `frame_000800.ppm` is the menu, `gameplay.png` /
`frame_001800.ppm` a Classic Game frame. The screenshots are emulator-side
proof, not screenshots from a physical Gizmondo device.
