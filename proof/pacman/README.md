# Native ARM Windows Mobile rendering proof

This is a headless PocketHLE run of the native ARM Windows Mobile / Pocket PC Pac-Man CAB used as a regression target for the message-pump and framebuffer path.

- Target: legacy ARM PE32 executable, 240×320 portrait.
- Result: the emulator reached the rendering path and advanced `frame_counter` from 0 to 5359 before exiting cleanly.
- `pacman-gameplay.png` is a PNG conversion of the eighth captured RGB565 framebuffer snapshot.
- The run used the release CLI and Unicorn ARM backend with an extended synthetic message budget (`5000`) and wrote eight changed-frame snapshots.

- The latest reproduction from the original WinZip self-extracting upload is recorded in `run.log`; it used the scheduled tap `1000:120,150`, reached `frame_counter=18050`, and exited with status 0.
- `pacman-gameplay-capture.png` shows the title screen after the startup tap; `pacman-gameplay.png` remains the earlier gameplay capture.
- The screenshots are emulator-side proof. A native Windows Mobile SDK/device emulator was not available in the host environment, so these are not screenshots from an actual handset.
