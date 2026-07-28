#!/usr/bin/env python3
"""Run a PocketHLE game with an ordered sequence of synthetic taps."""

from __future__ import annotations

import argparse
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
    parser.add_argument("--frames", type=Path, help="directory for PPM frames")
    parser.add_argument(
        "--dump-frames-to", type=Path,
        help="alias for --frames, matching the pockethle CLI spelling",
    )
    parser.add_argument("--max-frames", type=int, default=0)
    parser.add_argument("--message-budget", type=int, default=240)
    parser.add_argument("--max-slices", type=int, default=500_000)
    parser.add_argument("--instructions-per-slice", type=int, default=1_000_000)
    parser.add_argument("--pockethle", type=Path, default=Path("target/release/pockethle"))
    parser.add_argument("--dry-run", action="store_true", help="print the command without running it")
    args = parser.parse_args()

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
        x, separator, y = tap.partition(",")
        if not separator:
            parser.error(f"invalid --tap {tap!r}; expected X,Y")
        try:
            if not (0 <= int(x) <= 239 and 0 <= int(y) <= 319):
                raise ValueError
        except ValueError:
            parser.error(f"invalid --tap {tap!r}; coordinates must be X=0..239,Y=0..319")
        command.extend(("--tap", f"{int(x)},{int(y)}"))
    if frame_dir:
        command.extend(("--dump-frames-to", str(frame_dir)))
    if args.max_frames:
        command.extend(("--max-frames", str(args.max_frames)))

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
