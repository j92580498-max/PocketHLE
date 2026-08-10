#!/usr/bin/env python3
"""Build a PocketHLE compatibility database with the simple AppDB layout."""

from __future__ import annotations

import html
import json
import shutil
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "appdb-site"
SOURCE = ROOT / "appdb-site-source"
REPO = "https://github.com/j92580498-max/PocketHLE"
ISSUE_URL = f"{REPO}/issues/new?template=compatibility-report.yml"

GAMES = [
    {
        "slug": "asphalt-4-elite-racing", "name": "Asphalt 4: Elite Racing", "target": "Windows Mobile / WVGA",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 4, "result": "Gameplay rendered; PCM audio captured",
        "proof": "WVGA rendering reaches gameplay with a captured PCM audio track.", "image": "proof/asphalt4-wvga/asphalt4-gameplay-proof.png",
        "proof_url": f"{REPO}/blob/main/proof/asphalt4-wvga/README.md",
    },
    {
        "slug": "call-of-duty-2", "name": "Call of Duty 2", "target": "Windows Mobile / 240×320",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Boots through GLES to the title screen",
        "proof": "The software OpenGL ES layer reaches the title screen; gameplay is not yet claimed.", "image": "proof/cod2-gles/cod2-title-screen-landscape.png",
        "proof_url": f"{REPO}/blob/main/proof/cod2-gles/README.md",
    },
    {
        "slug": "asphalt-2-3d", "name": "Asphalt 2 3D", "target": "Windows Mobile / QVGA",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Continuous rendering and audio submission verified",
        "proof": "The Motorola Q9 run covers continuous rendering and submitted audio.", "image": "proof/asphalt2-3d/06-race-riding.png",
        "proof_url": f"{REPO}/blob/main/proof/asphalt2-3d/audio-verification.md",
    },
    {
        "slug": "jumpyball", "name": "JumpyBall", "target": "Pocket PC / QVGA",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Gameplay scene rendered; audio path verified",
        "proof": "The original ARM Windows Mobile target reaches an in-game scene.", "image": "proof/jumpyball/02-gameplay.png",
        "proof_url": f"{REPO}/blob/main/proof/jumpyball/README.md",
    },
    {
        "slug": "zenonia", "name": "Zenonia 1.6", "target": "Windows Mobile / 240×320",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Non-black gameplay scene rendered",
        "proof": "The Windows Mobile build produces a non-black gameplay scene after the startup taps.", "image": "proof/zenonia-windows-mobile/gameplay.png",
        "proof_url": f"{REPO}/blob/main/proof/zenonia-windows-mobile/README.md",
    },
    {
        "slug": "sky-force", "name": "Sky Force", "target": "Windows Mobile / 240×320",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Loading and language menu rendered",
        "proof": "Checked-in captures show frame progression through loading and the interactive language menu.", "image": "proof/skyforce/skyforce-gameplay-proof.png",
        "proof_url": f"{REPO}/blob/main/proof/skyforce/README.md",
    },
    {
        "slug": "pac-man", "name": "Pac-Man", "target": "Windows Mobile / 240×320",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Title screen and gameplay capture rendered",
        "proof": "Native ARM Windows Mobile rendering proof with an emulator-side gameplay capture.", "image": "proof/pacman/pacman-gameplay.png",
        "proof_url": f"{REPO}/blob/main/proof/pacman/README.md",
    },
    {
        "slug": "tank-ace-1944", "name": "Tank Ace 1944", "target": "Pocket PC / 240×320",
        "frontend": "CLI", "cpu": "ARM / Unicorn", "rating": 3, "result": "Rendered gameplay surface captured",
        "proof": "An ARM Unicorn run reaches the game rendering path and produces a gameplay surface.", "image": "proof/tank-ace-1944/gameplay.png",
        "proof_url": f"{REPO}/blob/main/proof/tank-ace-1944/README.md",
    },
]

RATING_DESCRIPTIONS = {
    1: "Completely broken: the app crashes immediately without any user interaction.",
    2: "Only the startup, intro, or part of the main menu is working.",
    3: "Some main content works, but with major issues or incomplete verification.",
    4: "The main content works with only small issues.",
    5: "Everything works. The app is fully usable.",
}


def esc(value: object) -> str:
    return html.escape(str(value), quote=True)


def stars(rating: int) -> str:
    return "⭐️" * rating


def relative(prefix: str, path: str) -> str:
    return f"{prefix}{path}"


def shell(title: str, body: str, prefix: str = "") -> str:
    home = relative(prefix, "index.html")
    style = relative(prefix, "style.css")
    script = relative(prefix, "script.js")
    return f'''<!doctype html>
<meta charset=utf-8>
<meta name=viewport content="width=device-width, initial-scale=1">
<title>{esc(title)} - PocketHLE compatibility database</title>
<link rel=stylesheet href="{style}">
<script src="{script}" defer></script>
<div id=breadcrumbs>
<a href="{REPO}">PocketHLE GitHub</a> &gt;
<h1><a href="{home}">PocketHLE compatibility database</a></h1>
</div>
<div id=sign-in-status-box>
<form action="{ISSUE_URL}" method=get><input type=submit value="Submit report"></form>
<form action="{REPO}" method=get><input type=submit value="GitHub"></form>
</div>
{body}
<hr>
<footer>
<p>PocketHLE is an open-source high-level emulator for Windows CE and Windows Mobile applications.</p>
<p>Reports on this page are PocketHLE results or emulator-side proof from the repository. A partial result is not presented as full playability.</p>
<p><a href="{REPO}/releases">Downloads</a> · <a href="{REPO}/issues">Issues</a> · <a href="{REPO}/blob/main/LICENSE">License</a></p>
</footer>
'''


def stats_table() -> str:
    counts = Counter(game["rating"] for game in GAMES)
    rows = "".join(
        f"<tr><td>{counts[rating]}</td><td>{stars(rating)}</td><td>{esc(description)}</td></tr>"
        for rating, description in RATING_DESCRIPTIONS.items()
    )
    return f"<h3>Legend/Stats</h3><table><thead><tr><th># of apps</th><th>Rating</th><th>Description</th></tr></thead><tbody>{rows}</tbody></table>"


def app_table(prefix: str) -> str:
    rows = "".join(
        f'''<tr><td><a href="{relative(prefix, f"apps/{game['slug']}/index.html")}">{esc(game['name'])}</a></td><td>{esc(game['target'])}</td><td>{esc(game['frontend'])}</td><td>{stars(game['rating'])}</td><td>{esc(game['result'])}</td></tr>'''
        for game in GAMES
    )
    return f'''<h3>List</h3><table class=searchable-table><thead><tr><th>App name</th><th>Target</th><th>Frontend</th><th>Best rating</th><th>Result</th></tr></thead><tbody>{rows}</tbody></table>'''


def build_index(title: str = "Apps") -> str:
    body = f"<h2>{title}</h2>{stats_table()}{app_table('')}"
    return shell(title, body)


def build_app(game: dict) -> str:
    prefix = "../../"
    image = relative(prefix, game["image"])
    body = f'''<div id=breadcrumbs-extra>&gt; Apps &gt; {esc(game['name'])}</div>
<h2>App</h2>
<table><tbody>
<tr><th>App name</th><td><a href="{relative(prefix, f"apps/{game['slug']}/index.html")}">{esc(game['name'])}</a></td></tr>
<tr><th>Target</th><td>{esc(game['target'])}</td></tr>
<tr><th>Frontend</th><td>{esc(game['frontend'])}</td></tr>
<tr><th>CPU backend</th><td>{esc(game['cpu'])}</td></tr>
<tr><th>Best rating</th><td>{stars(game['rating'])}</td></tr>
<tr><th>Result</th><td>{esc(game['result'])}</td></tr>
</tbody></table>
<h3>Reports</h3>
<table><thead><tr><th>Version</th><th>Frontend</th><th>CPU</th><th>Rating</th><th>Remarks</th><th>Proof</th></tr></thead><tbody>
<tr><td>PocketHLE current</td><td>{esc(game['frontend'])}</td><td>{esc(game['cpu'])}</td><td>{stars(game['rating'])}</td><td>{esc(game['proof'])}</td><td><a href="{game['proof_url']}">View</a></td></tr>
</tbody></table>
<h3>Legend</h3><table><thead><tr><th>Rating</th><th>Description</th></tr></thead><tbody>{''.join(f'<tr><td>{stars(rating)}</td><td>{esc(description)}</td></tr>' for rating, description in RATING_DESCRIPTIONS.items())}</tbody></table>
<h3>Screenshots</h3><figure><img class=report-screenshot src="{image}" alt="{esc(game['name'])} screenshot"><figcaption><a href="{game['proof_url']}">Open full proof on GitHub</a></figcaption></figure>'''
    return shell(game["name"], body, prefix)


def build_about() -> str:
    body = f'''<h2>About</h2><p>PocketHLE runs original Windows CE and Windows Mobile executables by executing guest ARM or MIPS code and implementing the system APIs they actually use on the host.</p><h3>Project links</h3><table><tbody><tr><th>Source code</th><td><a href="{REPO}">{REPO}</a></td></tr><tr><th>Latest release</th><td><a href="{REPO}/releases">GitHub Releases</a></td></tr><tr><th>Compatibility reports</th><td><a href="{ISSUE_URL}">Submit a report</a></td></tr></tbody></table>'''
    return shell("About", body)


def main() -> None:
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir()
    for filename in ("style.css", "script.js"):
        shutil.copy2(SOURCE / filename, OUT / filename)
    for game in GAMES:
        source = ROOT / game["image"]
        target = OUT / game["image"]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    (OUT / "index.html").write_text(build_index(), encoding="utf-8")
    (OUT / "games.html").write_text(build_index("Apps"), encoding="utf-8")
    (OUT / "about.html").write_text(build_about(), encoding="utf-8")
    for game in GAMES:
        path = OUT / "apps" / game["slug"] / "index.html"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(build_app(game), encoding="utf-8")
    (OUT / ".nojekyll").write_text("", encoding="utf-8")
    (OUT / "robots.txt").write_text("User-agent: *\nAllow: /\n", encoding="utf-8")
    (OUT / "README.md").write_text("# PocketHLE compatibility database\n\nStatic AppDB-style compatibility site generated by `tools/build-appdb.py`.\n", encoding="utf-8")
    (OUT / "site-data.json").write_text(json.dumps({"project": REPO, "apps": GAMES}, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote AppDB-style PocketHLE site to {OUT} ({len(GAMES)} apps)")


if __name__ == "__main__":
    main()
