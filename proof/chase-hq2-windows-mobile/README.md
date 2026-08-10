# Chase HQ2 Evolution — Windows Mobile proof

The supplied `ChaseHQ2Evo_v1.2-4026edfaa157.zip` was run with the ARM Unicorn backend and the required `tools/ai-tap-sequence.py` helper. The run reached the title screen, rendered the Chase HQ2 Evolution intro, accepted the two synthetic taps, and produced 20 changed framebuffer frames before stopping at the requested frame budget.

Command:

```text
tools/ai-tap-sequence.py /home/.z/chat-uploads/ChaseHQ2Evo_v1.2-4026edfaa157.zip --pockethle target/release/pockethle --cpu unicorn --max-slices 1000000 --instructions-per-slice 1000000 --message-budget 0 --tap 120,160 --tap 120,220 --dump-frames-to /tmp/chase-hq2-frames --max-frames 20
```

Result: exit status `0`, clean emulator exit, `frame_counter=20`, 20 captured PPM frames. The contact sheet is `final-gameplay.png`.
