#!/usr/bin/env python3
"""Regenerate src/sintab_numbers.h — a 16384-entry 16-bit sine table.
One full turn = 16384 binary angle units; value 16384 = +1.0 in 2.14 (<<2 -> 16.16)."""
import math
vals = [round(16384 * math.sin(i * 2 * math.pi / 16384)) for i in range(16384)]
vals = [max(-16384, min(16383, v)) for v in vals]
out = []
for i in range(0, 16384, 8):
    out.append("\t" + ", ".join(str(v) for v in vals[i:i+8]) + ",")
open("src/sintab_numbers.h", "w").write("\n".join(out) + "\n")
