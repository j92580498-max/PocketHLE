#!/usr/bin/env python3
"""Run a PocketHLE game with an ordered sequence of synthetic taps."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "AI/vision helper for PocketHLE: pass button coordinates selected "
            "by another program and capture the resulting framebuffer."
        )
    )
    parser.add_argument("game", type=Path, help="PocketHLE .exe, .cab, or .zip")
    parser.add_argument(
        "--cpu", choices=("unicorn", "mips", "stub"), default="unicorn",
        help="CPU backend: unicorn for ARM, mips for MIPS, or stub",
    )
    parser.add_argument(
        "--tap", action="append", default=[], metavar="X,Y",
        help="tap coordinate; repeat this option to press several buttons",
    )
    parser.add_argument(
        "--key", action="append", default=[], metavar="[FRAME:]KEY",
        help="virtual key; optionally prefix with the rendered frame number",
    )
    parser.add_argument("--frames", type=Path, help="directory for PPM frames")
    parser.add_argument(
        "--dump-frames-to", type=Path,
        help="alias for --frames, matching the pockethle CLI spelling",
    )
    parser.add_argument(
        "--dump-frame-stride", type=int, default=0,
        help="pass through to pockethle: write every Nth changed frame",
    )
    parser.add_argument("--max-frames", type=int, default=0)
    parser.add_argument("--message-budget", type=int, default=240)
    parser.add_argument("--screen", metavar="WIDTHxHEIGHT", help="emulated display geometry")
    parser.add_argument("--max-slices", type=int, default=500_000)
    parser.add_argument("--instructions-per-slice", type=int, default=1_000_000)
    parser.add_argument("--pockethle", type=Path, default=Path("target/release/pockethle"))
    parser.add_argument("--dry-run", action="store_true", help="print the command without running it")
    args = parser.parse_args()

    # Tap coordinates are validated against the emulated screen geometry
    # (default 240x320 QVGA, overridden by --screen WIDTHxHEIGHT). VGA /
    # WVGA builds such as Bubble Breaker need the full 480x640 range.
    screen_w, screen_h = 240, 320
    if args.screen:
        m = re.fullmatch(r"(\d+)[xX](\d+)", args.screen.strip())
        if not m:
            parser.error(f"invalid --screen {args.screen!r}; expected WIDTHxHEIGHT")
        screen_w, screen_h = int(m.group(1)), int(m.group(2))

    if not args.game.exists():
        parser.error(f"game file does not exist: {args.game}")
    if args.frames and args.dump_frames_to:
        parser.error("use either --frames or --dump-frames-to, not both")
    frame_dir = args.dump_frames_to or args.frames

    command = [
        str(args.pockethle), "run", str(args.game), "--cpu", args.cpu,
        "--max-slices", str(args.max_slices),
        "--instructions-per-slice", str(args.instructions_per_slice),
        "--message-budget", str(args.message_budget),
    ]
    for tap in args.tap:
        frame_prefix, separator, coordinates = tap.partition(":")
        if separator and not frame_prefix.isdigit():
            parser.error(f"invalid --tap {tap!r}; frame prefix must be an integer")
        coordinate_text = coordinates if separator else tap
        x, coordinate_separator, y = coordinate_text.partition(",")
        if not coordinate_separator:
            parser.error(f"invalid --tap {tap!r}; expected [FRAME:]X,Y")
        try:
            if not (0 <= int(x) < screen_w and 0 <= int(y) < screen_h):
                raise ValueError
        except ValueError:
            parser.error(
                f"invalid --tap {tap!r}; coordinates must be X=0..{screen_w - 1},Y=0..{screen_h - 1}"
            )
        normalized = f"{int(x)},{int(y)}"
        if separator:
            normalized = f"{int(frame_prefix)}:{normalized}"
        command.extend(("--tap", normalized))
    for key in args.key:
        command.extend(("--key", key))
    if frame_dir:
        command.extend(("--dump-frames-to", str(frame_dir)))
    if args.dump_frame_stride:
        command.extend(("--dump-frame-stride", str(args.dump_frame_stride)))
    if args.max_frames:
        command.extend(("--max-frames", str(args.max_frames)))
    if args.screen:
        command.extend(("--screen", args.screen))

    print("AI tap command:")
    print(" ".join(subprocess.list2cmdline([part]) for part in command))
    if args.dry_run:
        return 0
    if not args.pockethle.exists():
        print(f"error: PocketHLE binary not found: {args.pockethle}", file=sys.stderr)
        print("build it with: cargo build --release -p pocket-cli --features unicorn", file=sys.stderr)
        return 2
    return subprocess.run(command, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
