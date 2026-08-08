# Trailblazer (Gizmondo / Windows Mobile) proof

This proof captures the uploaded `Trailblazer_Gizmondo_EN-f6b2daf85a01.zip` with the ARM Unicorn backend after the startup and worker-thread fixes.

Command:

```text
python3 tools/ai-tap-sequence.py /home/.z/chat-uploads/Trailblazer_Gizmondo_EN-f6b2daf85a01.zip --cpu unicorn --pockethle target/release/pockethle --tap 120,160 --frames proof/trailblazer-gizmondo --max-frames 8 --max-slices 2000000 --instructions-per-slice 1000000 --message-budget 240
```

Result: exit status 0, eight valid 240x320 PPM frames, `frame_counter=14`, and clean emulator shutdown. The startup log reaches EGL initialization, creates the 240x320 RGB565 surface, starts the audio worker, schedules worker threads, and continues past the previous `SetSystemMemoryDivision` failure.

The final frame is a rendered Trailblazer title/menu capture. The run is an emulator-side Windows Mobile compatibility proof, not a native-device capture.
