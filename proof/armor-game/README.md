# Armor Game proof

The supplied `Armor.exe` was run through PocketHLE's ARM/Unicorn backend with the software WinCE API layer. The run reached the live game UI and rendered the expected controls from the embedded resources:

- `gameplay-controls.png` — game window with `Fire` and `New Game` controls;
- `gameplay-full-controls.png` — game window with both `+` / `-` control pairs and the bottom action controls;
- `run.log` — run diagnostics; `frame_counter=165` at clean exit;
- `run-trace.jsonl` — API trace showing resource/window/control initialization and no startup `MessageBoxW`.

The original failing run stopped in the startup error dialog `Couldn't create MenuBar`; `commctrl.dll` ordinal 12 was returning a generic success without the legacy menu-bar output structure, and `aygshell.dll` ordinal 34 was also treated as a generic stub. The fix models the legacy output structure for both paths, so the application continues to create its game controls and render frames.
