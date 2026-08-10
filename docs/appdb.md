# PocketHLE AppDB

PocketHLE publishes a static compatibility catalog at [j92580498-max.github.io/PocketHLE](https://j92580498-max.github.io/PocketHLE/). It keeps the table-based layout and browser search of the touchHLE AppDB while removing the PHP server, database, login system, and always-on hosting dependency.

## How it works

- `tools/build-appdb.py` downloads the public AppDB index, app pages, stylesheet, JavaScript, privacy page, and referenced screenshots.
- The generator rewrites internal links for GitHub Pages and points report forms to the PocketHLE GitHub issue form.
- GitHub Actions rebuilds and deploys the site weekly, manually, and after generator changes.
- The published site is a static GitHub Pages artifact. No process needs to stay alive, so a stopped PHP server cannot take it down.
- Imported compatibility reports are touchHLE reference data, not PocketHLE test results until someone verifies them with PocketHLE.

## First-time GitHub setup

The repository owner must enable Pages once in **Settings → Pages**, selecting **GitHub Actions** as the source. After the workflow completes, the expected URL is `https://j92580498-max.github.io/PocketHLE/`.

GitHub may pause scheduled workflows in repositories with no activity for a long period. That only pauses refreshes: the already-deployed static site remains available. A push or manual workflow run resumes publishing.

## Attribution

The original database application is [hikari-no-yume/app-compatibility-db](https://github.com/hikari-no-yume/app-compatibility-db). Database content is attributed on the mirrored pages under the original CC BY 4.0 terms; screenshots remain subject to the copyright of their respective apps.
