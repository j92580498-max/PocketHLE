# PocketHLE website

PocketHLE publishes a project website at [j92580498-max.github.io/PocketHLE](https://j92580498-max.github.io/PocketHLE/). It is a static GitHub Pages site about the emulator itself: architecture, downloads, and checked-in compatibility proof.

## How it works

- `tools/build-appdb.py` builds three pages from the repository: the home page, compatibility gallery, and project overview.
- `appdb-site-source/` contains the small presentation layer; proof screenshots and notes are copied from `proof/`.
- `appdb-site/` is the generated deploy directory and is published from the `gh-pages` branch.
- The site does not run a server, database, PHP process, or scheduled host. GitHub Pages serves static files, so the website does not expire after a month because a process stopped.
- Compatibility language is intentionally conservative: a title-screen boot or emulator-side capture is not described as full playability unless the repository proof says so.

## Updating the site

Run:

```bash
python3 tools/build-appdb.py
```

Then review the generated pages locally and update the `gh-pages` branch with the contents of `appdb-site/`. Keep the source generator and generated artifact in the same pull request when changing the site.

## Hosting

The current Pages source is the `gh-pages` branch at the repository root. The public URL is `https://j92580498-max.github.io/PocketHLE/`.

GitHub Pages is a static hosting service. It does not need an always-on VM or a monthly renewal workflow. GitHub can pause scheduled Actions jobs after long inactivity, but that only affects optional rebuilds; it does not remove the already-published static files.
