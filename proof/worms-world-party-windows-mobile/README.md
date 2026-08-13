# Worms World Party — Windows Mobile proof

The supplied `wwp.CAB` payload was run through PocketHLE's ARM Unicorn backend with the repository's `tools/ai-tap-sequence.py` workflow.

## Result

- `frame_counter`: **41**
- Captured frames: **20**
- Emulator exit: **clean, status 0**
- Automated taps: **3 × (120,200)**
- Screen: **320×240** landscape
- Rendered output: non-black Worms World Party title screen with logo, artwork, and Jamdat UI elements

## Fix

The executable imports `_wcsupr` from `coredll.dll` during its Windows Mobile startup path. PocketHLE registered `CharUpperW` but not the CRT-compatible `_wcsupr` alias, so the import was dispatched as an unimplemented call and the ARM run stopped before the first frame (`frame_counter=0`). The fix registers `_wcsupr` against the existing wide-string uppercase implementation.

The two supplied executable variants (`wwp.CAB` and `wwp-2003.CAB`) were both reproduced at the same failure point. The successful proof run used the combined executable/resource directory assembled from the supplied CABs so that the launcher sees the real `WWP.RB` resource payload under the install path.

## Limitations

This is emulator-side proof using ARM Unicorn; no native Windows Mobile device or legacy Windows Mobile SDK emulator is available in this environment. The supplied payload reaches and renders its title screen, but the included tap sequence does not transition beyond the title artwork in this build.
