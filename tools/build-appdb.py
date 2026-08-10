#!/usr/bin/env python3
"""Build the PocketHLE project and compatibility website."""

from __future__ import annotations

import html
import json
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "appdb-site"
SOURCE = ROOT / "appdb-site-source"
REPO = "https://github.com/j92580498-max/PocketHLE"
PAGES = "https://j92580498-max.github.io/PocketHLE"
ISSUE_URL = f"{REPO}/issues/new?template=compatibility-report.yml"

GAMES = [
    {
        "slug": "asphalt-4-elite-racing",
        "name": "Asphalt 4: Elite Racing",
        "status": "Rendering + audio proof",
        "proof": "WVGA rendering reaches gameplay with a captured PCM audio track.",
        "image": "proof/asphalt4-wvga/asphalt4-gameplay-proof.png",
        "readme": "proof/asphalt4-wvga/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/asphalt4-wvga/README.md",
    },
    {
        "slug": "call-of-duty-2",
        "name": "Call of Duty 2",
        "status": "Title screen verified",
        "proof": "Boots through the software OpenGL ES layer to the title screen.",
        "image": "proof/cod2-gles/cod2-title-screen-landscape.png",
        "readme": "proof/cod2-gles/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/cod2-gles/README.md",
    },
    {
        "slug": "asphalt-2-3d",
        "name": "Asphalt 2 3D",
        "status": "Rendering + audio path proof",
        "proof": "The Motorola Q9 run covers continuous rendering and submitted audio.",
        "image": "proof/asphalt2-3d/06-race-riding.png",
        "readme": "proof/asphalt2-3d/audio-verification.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/asphalt2-3d/audio-verification.md",
    },
    {
        "slug": "jumpyball",
        "name": "JumpyBall",
        "status": "Gameplay verified",
        "proof": "The original ARM Windows Mobile target reaches an in-game scene.",
        "image": "proof/jumpyball/02-gameplay.png",
        "readme": "proof/jumpyball/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/jumpyball/README.md",
    },
    {
        "slug": "zenonia",
        "name": "Zenonia 1.6",
        "status": "Gameplay verified",
        "proof": "The Windows Mobile build produces a non-black gameplay scene.",
        "image": "proof/zenonia-windows-mobile/gameplay.png",
        "readme": "proof/zenonia-windows-mobile/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/zenonia-windows-mobile/README.md",
    },
    {
        "slug": "sky-force",
        "name": "Sky Force",
        "status": "Rendering proof",
        "proof": "Checked-in captures show frame progression through loading and gameplay.",
        "image": "proof/skyforce/skyforce-gameplay-proof.png",
        "readme": "proof/skyforce/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/skyforce/README.md",
    },
    {
        "slug": "pac-man",
        "name": "Pac-Man",
        "status": "Gameplay capture",
        "proof": "Native ARM Windows Mobile rendering proof with an emulator-side gameplay capture.",
        "image": "proof/pacman/pacman-gameplay.png",
        "readme": "proof/pacman/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/pacman/README.md",
    },
    {
        "slug": "tank-ace-1944",
        "name": "Tank Ace 1944",
        "status": "Gameplay capture",
        "proof": "An ARM Unicorn run produces a rendered gameplay surface.",
        "image": "proof/tank-ace-1944/gameplay.png",
        "readme": "proof/tank-ace-1944/README.md",
        "proof_url": "https://github.com/j92580498-max/PocketHLE/blob/main/proof/tank-ace-1944/README.md",
    },
]

FEATURES = [
    ("High-level emulation", "Run original Windows CE / Windows Mobile executables without shipping Microsoft system DLLs."),
    ("ARM + MIPS", "Load PE32 applications through the Unicorn CPU backend, with a trace-only stub backend for diagnostics."),
    ("GAPI + OpenGL ES", "Render legacy framebuffer titles and software OpenGL ES 1.x games through the same core."),
    ("Desktop + Android", "Use the CLI for reproducible runs, the egui desktop launcher, or the Android game library."),
]


def esc(value: str) -> str:
    return html.escape(value, quote=True)


def nav(active: str) -> str:
    links = [("home", "Home", "index.html"), ("games", "Compatibility", "games.html"), ("about", "About", "about.html")]
    return "".join(
        f'<a class="nav-link {"active" if key == active else ""}" href="{href}">{label}</a>'
        for key, label, href in links
    )


def layout(title: str, active: str, body: str) -> str:
    return f'''<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="description" content="PocketHLE — a high-level Windows Mobile and Pocket PC emulator.">
  <meta property="og:title" content="{esc(title)} — PocketHLE">
  <meta property="og:description" content="A high-level emulator for Windows Mobile and Pocket PC games.">
  <title>{esc(title)} — PocketHLE</title>
  <link rel="icon" href="logo.png">
  <link rel="stylesheet" href="style.css">
  <script defer src="script.js"></script>
</head>
<body>
  <div class="site-grid"></div>
  <header class="site-header wrap">
    <a class="brand" href="index.html"><span class="brand-mark">P</span><span>Pocket<span class="brand-accent">HLE</span></span></a>
    <nav class="nav">{nav(active)}</nav>
    <a class="github-link" href="{REPO}" target="_blank" rel="noreferrer">GitHub <span>↗</span></a>
  </header>
  <main>{body}</main>
  <footer class="site-footer wrap"><span>POCKETHLE / OPEN SOURCE WINDOWS MOBILE HLE</span><span><a href="{REPO}/releases">Releases</a> · <a href="{REPO}/issues">Issues</a> · <a href="{REPO}/blob/main/LICENSE">License</a></span></footer>
</body>
</html>'''


def game_card(game: dict, featured: bool = False) -> str:
    return f'''<article class="game-card{' featured' if featured else ''}">
  <a class="game-image" href="{game["proof_url"]}"><img src="{game["image"]}" alt="{esc(game["name"])} proof screenshot" loading="lazy"></a>
  <div class="game-card-body"><div class="eyebrow">{esc(game["status"])}</div><h3>{esc(game["name"])}</h3><p>{esc(game["proof"])}</p><a class="text-link" href="{game["proof_url"]}" target="_blank" rel="noreferrer">View proof <span>↗</span></a></div>
</article>'''


def copy_proof_assets() -> None:
    for game in GAMES:
        source = ROOT / game["image"]
        target = OUT / game["image"]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def build_home() -> str:
    feature_markup = "".join(f'<div class="feature"><span class="feature-number">0{i + 1}</span><div><h3>{esc(title)}</h3><p>{esc(text)}</p></div></div>' for i, (title, text) in enumerate(FEATURES))
    proof_markup = "".join(game_card(game, i == 0) for i, game in enumerate(GAMES[:4]))
    return layout("Windows Mobile HLE", "home", f'''<section class="hero wrap">
  <div class="hero-copy"><div class="eyebrow">WINDOWS MOBILE / POCKET PC</div><h1>Old games.<br><em>New hardware.</em></h1><p class="hero-lead">PocketHLE is a high-level emulator for Windows CE and Windows Mobile games. It runs the original executable and reimplements the APIs it needs on the host.</p><div class="hero-actions"><a class="button button-primary" href="{REPO}/releases/latest">Download latest <span>↗</span></a><a class="button button-quiet" href="games.html">Compatibility <span>→</span></a></div></div>
  <div class="hero-art"><div class="device-frame"><img src="proof/cod2-gles/cod2-title-screen-landscape.png" alt="Call of Duty 2 running in PocketHLE"><div class="device-label">POCKETHLE / 0.2.0</div></div><div class="hero-stamp">HLE<br><span>01</span></div></div>
</section>
<section class="signal-strip"><div class="wrap signal-inner"><div><strong>v0.2.0</strong><span>latest release</span></div><div><strong>ARM / MIPS</strong><span>CPU backends</span></div><div><strong>DESKTOP / ANDROID</strong><span>frontends</span></div><div><strong>OPEN SOURCE</strong><span>MIT or Apache-2.0</span></div></div></section>
<section class="features wrap"><div class="section-heading"><div class="eyebrow">THE APPROACH</div><h2>Small, focused,<br>and honest.</h2><p>Not a full Windows CE virtual machine. PocketHLE intercepts the boundary between a game and its system DLLs, then implements only what the game actually uses.</p></div><div class="feature-list">{feature_markup}</div></section>
<section class="proof-section wrap"><div class="section-heading row-heading"><div><div class="eyebrow">COMPATIBILITY</div><h2>Evidence, not promises.</h2></div><a class="text-link" href="games.html">See all compatibility <span>→</span></a></div><div class="game-grid">{proof_markup}</div></section>
<section class="cta wrap"><div><div class="eyebrow">BUILD WITH US</div><h2>Bring a game.<br>Leave a trace.</h2></div><div><p>Every new compatibility result belongs in the repository as a reproducible run, a frame capture, or a focused API fix.</p><a class="button button-dark" href="{ISSUE_URL}">Report compatibility <span>↗</span></a></div></section>''')


def build_games() -> str:
    cards = "".join(game_card(game) for game in GAMES)
    return layout("Compatibility", "games", f'''<section class="page-hero wrap"><div class="eyebrow">POCKETHLE COMPATIBILITY</div><h1>What runs,<br><em>and how far.</em></h1><p>These entries link to reproducible proof checked into the PocketHLE repository. A title-screen boot is not called full playability, and a capture is not a promise for every device.</p></section>
<section class="compatibility wrap"><div class="compat-toolbar"><div class="eyebrow">{len(GAMES):02d} DOCUMENTED RESULTS</div><label class="search"><span>⌕</span><input id="game-search" type="search" placeholder="Filter titles" autocomplete="off"></label></div><div class="game-grid" id="game-grid">{cards}</div><div class="empty-search" id="empty-search" hidden>No matching proof yet.</div></section>
<section class="report-band"><div class="wrap report-inner"><div><div class="eyebrow">YOUR TURN</div><h2>Test another title.</h2></div><div><p>Use a legally obtained game, record the version and frontend, and attach the frame or log that proves the result.</p><a class="button button-primary" href="{ISSUE_URL}">Submit a report <span>↗</span></a></div></div></section>''')


def build_about() -> str:
    return layout("About PocketHLE", "about", f'''<section class="page-hero wrap"><div class="eyebrow">ABOUT THE PROJECT</div><h1>A clean-room<br><em>shortcut.</em></h1><p>PocketHLE does not emulate a whole Windows CE device. It loads the real game PE, executes its ARM or MIPS code, and services imported system calls with host-side Rust.</p></section>
<section class="about-layout wrap"><div class="about-main"><div class="eyebrow">ARCHITECTURE</div><h2>The game stays real.<br>The operating system gets focused.</h2><div class="architecture"><div class="arch-node"><span>01</span><strong>PE loader</strong><small>Maps the original executable and resolves imports.</small></div><div class="arch-line"></div><div class="arch-node"><span>02</span><strong>CPU backend</strong><small>Runs guest ARM or MIPS instructions through Unicorn.</small></div><div class="arch-line"></div><div class="arch-node"><span>03</span><strong>HLE API layer</strong><small>Reimplements coredll, GAPI, GLES, audio, files, and more.</small></div><div class="arch-line"></div><div class="arch-node"><span>04</span><strong>Frontend</strong><small>Presents frames and input on CLI, desktop, or Android.</small></div></div></div><aside class="about-side"><div class="eyebrow">GET STARTED</div><h3>Latest release</h3><p>Download the ready-made build for your platform, or build from source with Rust.</p><a class="button button-primary full" href="{REPO}/releases/latest">Open releases <span>↗</span></a><a class="button button-outline full" href="{REPO}">Read the source <span>↗</span></a><hr><div class="eyebrow">PLATFORMS</div><ul class="plain-list"><li>Linux x86_64</li><li>Windows x86_64</li><li>Android arm64 / armeabi-v7a</li></ul></aside></section>
<section class="roadmap wrap"><div class="section-heading"><div class="eyebrow">ROADMAP</div><h2>More games,<br>fewer mysteries.</h2></div><div class="roadmap-list"><div><span>01</span><p>Expand CRT startup and dynamic import coverage.</p></div><div><span>02</span><p>Improve resource, GDI, dialog, input, and gamepad support.</p></div><div><span>03</span><p>Turn more compatibility probes into reproducible proof captures.</p></div></div></section>
<section class="cta wrap"><div><div class="eyebrow">LEGAL BY DESIGN</div><h2>No system DLLs.<br>No game assets.</h2></div><div><p>PocketHLE is a clean-room reimplementation. Users provide their own legally obtained games and archives.</p><a class="text-link" href="{REPO}/blob/main/README.md">Read the legal notice <span>↗</span></a></div></section>''')


def main() -> None:
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir()
    for source in ("style.css", "script.js"):
        shutil.copy2(SOURCE / source, OUT / source)
    shutil.copy2(ROOT / "frontends/pocket-desktop/assets/pockethle-logo.png", OUT / "logo.png")
    copy_proof_assets()
    write_pages = {"index.html": build_home(), "games.html": build_games(), "about.html": build_about()}
    for path, content in write_pages.items():
        (OUT / path).write_text(content, encoding="utf-8")
    (OUT / ".nojekyll").write_text("", encoding="utf-8")
    (OUT / "robots.txt").write_text("User-agent: *\nAllow: /\n", encoding="utf-8")
    (OUT / "README.md").write_text("# PocketHLE website\n\nGenerated static project site for PocketHLE. Source: `tools/build-appdb.py` and `appdb-site-source/`.\n", encoding="utf-8")
    (OUT / "site-data.json").write_text(json.dumps({"project": REPO, "games": GAMES}, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote PocketHLE site to {OUT} ({len(write_pages)} pages, {len(GAMES)} compatibility entries)")


if __name__ == "__main__":
    main()
