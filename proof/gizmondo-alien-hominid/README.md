# Alien Hominid Gizmondo startup proof

The capture was produced from `Alien-Hominid_Gizmondo_EN_Beta-a611bf2b4bac.zip` with the ARM Unicorn backend and the repository helper:

```text
python3 tools/ai-tap-sequence.py \
  Alien-Hominid_Gizmondo_EN_Beta-a611bf2b4bac.zip \
  --cpu unicorn \
  --tap 120,160 --tap 120,220 \
  --dump-frames-to proof/gizmondo-alien-hominid/frames \
  --max-frames 8
```

The run exited cleanly and reported `frame_counter=8`. `ai-tap-sequence.log` contains the full output. The eight PPM frames are emulator framebuffer captures; `reference-gameplay.png` is the existing visual reference for the expected Gizmondo gameplay surface.
