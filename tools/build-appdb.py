#!/usr/bin/env python3
"""Build a static PocketHLE AppDB mirror from the public touchHLE database."""

from __future__ import annotations

import re
import shutil
import urllib.request
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "appdb-site"
SOURCE = "https://appdb.touchhle.org"
ISSUE_URL = "https://github.com/j92580498-max/PocketHLE/issues/new?template=compatibility-report.yml"


def fetch(url: str) -> tuple[str, bytes]:
    request = urllib.request.Request(url, headers={"User-Agent": "PocketHLE-AppDB-builder/1.0"})
    with urllib.request.urlopen(request, timeout=45) as response:
        return response.headers.get("content-type", ""), response.read()


class AppLinkParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.links: set[str] = set()

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag == "a":
            href = dict(attrs).get("href") or ""
            if re.fullmatch(r"/apps/\d+", href):
                self.links.add(href)


def page_path(source_path: str) -> Path:
    if source_path == "/":
        return Path("index.html")
    if match := re.fullmatch(r"/apps/(\d+)", source_path):
        return Path("apps") / match.group(1) / "index.html"
    return Path(source_path.lstrip("/"))


def relative_asset(source_path: str, current: Path) -> str:
    target = page_path(source_path)
    return str(Path(__import__("os").path.relpath(target, current.parent))).replace("\\", "/")


def rewrite_html(source: str, current: Path) -> str:
    def rewrite_attribute(match: re.Match[str]) -> str:
        prefix, quote, value = match.groups()
        if value.startswith("/style.css"):
            value = relative_asset("/style.css", current)
        elif value.startswith("/script.js"):
            value = relative_asset("/script.js", current)
        elif value.startswith("/apps/"):
            value = relative_asset(value.rstrip("/"), current)
        elif value.startswith("/reports/") and value.endswith("/screenshot"):
            report_id = value.split("/")[2]
            value = relative_asset(f"/reports/{report_id}/screenshot.jpg", current)
        elif value == "/privacy.html":
            value = relative_asset(value, current)
        elif value == "/":
            value = relative_asset(value, current)
        return f"{prefix}{quote}{value}{quote}"

    result = re.sub(r'((?:href|src)\s*=\s*)(["\'])(/[^"\']*)\2', rewrite_attribute, source, flags=re.I)
    result = result.replace("href=/style.css", f"href={relative_asset('/style.css', current)}")
    result = result.replace("src=/script.js", f"src={relative_asset('/script.js', current)}")
    result = result.replace("href=/", f"href={relative_asset('/', current)}")
    result = result.replace('action="/signin"', f'action="{ISSUE_URL}"')
    result = result.replace('action="/reports/new"', f'action="{ISSUE_URL}"')
    result = result.replace("action='/signin'", f"action='{ISSUE_URL}'")
    result = result.replace("action='/reports/new'", f"action='{ISSUE_URL}'")
    result = result.replace("touchHLE app compatibility database", "PocketHLE app compatibility database")
    notice = (
        '<div class="unapproved"><strong>Reference catalog:</strong> '
        'these compatibility reports were imported from the public touchHLE database. '
        'Use the <a href="' + ISSUE_URL + '">GitHub issue form</a> to submit a PocketHLE report.</div>'
    )
    if "id=sign-in-status-box" in result:
        marker = "</div>"
        position = result.find(marker, result.find("id=sign-in-status-box"))
        if position != -1:
            position += len(marker)
            result = result[:position] + notice + result[position:]
    return result


def write_html(source_path: str, data: bytes) -> None:
    current = page_path(source_path)
    content = data.decode("utf-8", "replace")
    destination = OUT / current
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(rewrite_html(content, current), encoding="utf-8")


def main() -> None:
    _, home_bytes = fetch(SOURCE + "/")
    home = home_bytes.decode("utf-8", "replace")
    parser = AppLinkParser()
    parser.feed(home)
    paths = sorted(parser.links, key=lambda value: int(value.rsplit("/", 1)[1]))
    print(f"Found {len(paths)} app pages")

    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir()
    write_html("/", home_bytes)

    for index, path in enumerate(paths, start=1):
        _, data = fetch(SOURCE + path)
        write_html(path, data)
        if index % 50 == 0 or index == len(paths):
            print(f"Downloaded {index}/{len(paths)} app pages")

    for path in ["/style.css", "/script.js", "/privacy.html"]:
        _, data = fetch(SOURCE + path)
        (OUT / path.lstrip("/")).write_bytes(data)

    screenshot_ids: set[str] = set(re.findall(r"/reports/(\d+)/screenshot", home))
    for path in paths:
        source = (OUT / page_path(path)).read_text(encoding="utf-8")
        screenshot_ids.update(re.findall(r"reports/(\d+)/screenshot\.jpg", source))
    for index, report_id in enumerate(sorted(screenshot_ids, key=int), start=1):
        try:
            _, image = fetch(f"{SOURCE}/reports/{report_id}/screenshot")
            destination = OUT / "reports" / report_id / "screenshot.jpg"
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(image)
        except Exception as error:
            print(f"warning: could not copy screenshot {report_id}: {error}")
        if index % 100 == 0 or index == len(screenshot_ids):
            print(f"Downloaded {index}/{len(screenshot_ids)} screenshots")

    (OUT / ".nojekyll").write_text("", encoding="utf-8")
    (OUT / "robots.txt").write_text("User-agent: *\nAllow: /\n", encoding="utf-8")
    (OUT / "README.md").write_text(
        "# PocketHLE AppDB\n\n"
        "Static mirror of the public touchHLE app compatibility database, adapted for PocketHLE.\n\n"
        "The site is generated by `tools/build-appdb.py` and deployed with GitHub Pages. "
        "Compatibility reports are reference data until independently tested with PocketHLE.\n",
        encoding="utf-8",
    )
    print(f"Wrote static site to {OUT}")


if __name__ == "__main__":
    main()
