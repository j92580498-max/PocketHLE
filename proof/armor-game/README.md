# Armor Game proof

The supplied `Armor.exe` was run through PocketHLE's ARM/Unicorn backend with the software WinCE API layer. The run reached the live game UI, accepted a synthetic tap on **New Game**, and rendered the running battlefield with two tanks and live HUD counters:

- `gameplay-controls.png` — game window with `Fire` and `New Game` controls;
- `gameplay-full-controls.png` — game window with both `+` / `-` control pairs and the bottom action controls;
- `gameplay-running.png` — post-**New Game** gameplay proof: battlefield, player/enemy tanks, angle, velocity, human and computer counters;
- `run.log` — run diagnostics; `frame_counter=165` at clean exit;
- `run-trace.jsonl` — API trace showing resource/window/control initialization and no startup `MessageBoxW`;
- `gameplay-run.log` — run log proving the tap was delivered and `WM_COMMAND` id 106 (**New Game**) reached the guest window procedure.

The original failing run stopped in the startup error dialog `Couldn't create MenuBar`; `commctrl.dll` ordinal 12 was returning a generic success without the legacy menu-bar output structure, and `aygshell.dll` ordinal 34 was also treated as a generic stub. The fix models the legacy output structure for both paths, so the application continues to create its game controls and render frames.

The gameplay proof run used `--tap 10:50,255` and continued until 80 rendered frames. The log records `control id=106 clicked` followed by `WM_COMMAND` `0x111`; the final frame shows the active battlefield rather than the pre-game blank field.
