# Street Duel — Windows Mobile proof

## Root cause

Street Duel decodes its packed `data.bin` resources through the C runtime `memset` import. PocketHLE reused one scratch buffer for these calls, but the growth path only initialized the newly appended tail. When a later, larger memset requested a different byte value, the old prefix remained stale. Street Duel then rejected the decoded `FONT.tga` resource as not being an uncompressed 24-bit TGA and dereferenced an invalid texture state before its first rendered frame.

The fix grows the scratch buffer with zeroes and fills the complete requested prefix on every call. The change also makes the existing AYGSHELL handlers reachable through `LoadLibraryW` and `GetProcAddressA/W`, which Street Duel uses for fullscreen setup.

## Acceptance run

```sh
RUST_LOG=warn ./target/release/pockethle run Street_Duel_1_07.cab \
  --cpu unicorn \
  --tap 1:120,160 \
  --key 1:enter \
  --max-slices 0 \
  --instructions-per-slice 100000 \
  --dump-frames-to /tmp/street-duel-frames \
  --max-frames 100
```

Result: **PASS**. The optimized Linux CLI exited with code 0, rendered 102 frames, wrote 100 framebuffer snapshots, and reached the Street Duel title/menu sequence after the copyright and Pixelogic splash screens. The framebuffer was 240×320 RGB565 and no longer stopped at the old `READ unmapped 0x00004400` failure.

The CAB and game executable are test inputs supplied by the user and are intentionally not committed to this repository.

## Regression coverage

`memset_growing_scratch_is_filled_with_the_new_value` covers both buffer growth and a subsequent smaller overwrite. The full workspace suite passes with 383 tests passed, 10 ignored, and 0 failures; formatting and Clippy with warnings denied also pass.
