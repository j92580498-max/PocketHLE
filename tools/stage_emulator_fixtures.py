from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SUITES = ("ramtest", "vfstest", "timetest", "camtest", "gpstest", "nettest", "vibtest", "bttest")
FILES = {
    "ramtest": (
        "GZRT999999/AUTORUN.EXE",
        "GZRT999999/proctest.exe",
        "GZRT999999/procworker.exe",
        "GZRT999999/badproc.exe",
        "GZRT999999/pageprobe.dll",
        "GZRT999999/ramprobe.dll",
        "GZRT999999/ramprobe2.dll",
        "GZRT999999/ramreject.dll",
        "GZRT999999/depleaf.dll",
        "GZRT999999/deproot.dll",
        "GZRT999999/deppeer.dll",
        "GZRT999999/depreject.dll",
        "GZRT999999/depmissing.dll",
        "GZRT999999/depbadexport.dll",
        "GZRT999999/depcyclea.dll",
        "GZRT999999/depcycleb.dll",
        "fixtures/explicit-exit.exe",
        "fixtures/last-worker-exit.exe",
    ),
    "vfstest": ("GZVT999998/AUTORUN.EXE", "GZVT999998/vfsworker.exe", "GZVT999998/asset.bin"),
    "timetest": ("GZTT999995/AUTORUN.EXE",),
    "camtest": ("GZCT999997/AUTORUN.EXE",),
    "gpstest": ("GZGP999994/AUTORUN.EXE",),
    "nettest": ("GZNT999993/AUTORUN.EXE",),
    "vibtest": ("GZVB999990/AUTORUN.EXE",),
    "bttest": ("GZBT999996/AUTORUN.EXE",),
}

for suite in SUITES:
    subprocess.run([sys.executable, str(ROOT / "tools" / suite / "build.py")], cwd=ROOT, check=True)

with tempfile.TemporaryDirectory(prefix="pockethle-fixtures-") as temp_dir:
    staged = Path(temp_dir)
    for suite, paths in FILES.items():
        for relative_path in paths:
            source = ROOT / "tools" / suite / "dist" / relative_path
            if not source.is_file():
                raise FileNotFoundError(source)
            destination = staged / suite / relative_path
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)

    fixture_root = ROOT / "crates" / "pocket-core" / "tests" / "fixtures"
    if fixture_root.exists():
        shutil.rmtree(fixture_root)
    shutil.copytree(staged, fixture_root)
    (fixture_root / "README.md").write_text(
        "These ARM PE files are checked-in inputs for the `pocket-core` guest integration tests. "
        "Regenerate them from the diagnostic sources with `python3 tools/stage_emulator_fixtures.py`; "
        "Python, Clang and LLD are required. The intermediate build outputs under `tools/*/dist` remain ignored.\n"
    )

print(fixture_root)
