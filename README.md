# Printables Offline

**Version 2.0 — a macOS-only, account-aware, two-way library for your own Printables.com models.**

Printables Offline keeps a real copy of your 3D models on your Mac: the files, the images, the
description, the tags — and the entire edit history of every model folder, tracked in git.

> 2.0 is a substantial change of direction. 1.x was an anonymous, read-only, one-model-at-a-time
> downloader. 2.0 signs in to your Printables account, syncs every model you authored, puts each
> model folder under version control, and can push reviewed local edits back to the website.
> Windows support was removed in 2.0 — this is a macOS app.

---

## What it does

| | |
|---|---|
| **Sign in** | A real login window opens at printables.com, so Google/Apple SSO and 2FA work normally. The session is stored in the macOS Keychain. |
| **Sync all your models** | Lists every model you authored and pulls them in parallel, resuming where it left off and skipping files that are already correct. |
| **Local git** | Every model folder is its own git repository. Imports, pulls, local edits and pushes are all commits you can browse, diff and restore. |
| **Edit locally** | The folder *is* the model: `metadata.json` plus `files/` and `images/`. Change them however you like — the app notices, commits, and offers to publish. |
| **Push reviewed changes** | A change list (added / modified / deleted files, metadata edits) is shown before anything is sent. Nothing is written to printables.com without an explicit confirmation. |
| **Offline library** | Browse, search, filter, collections, open in your slicer, export — all without a network. |

## Requirements

- macOS 12 or newer (Apple Silicon or Intel)
- A slicer application, optional, for **Open in Slicer**
- A Printables account, optional, for sync and login

## Install

Download the latest `.dmg` from [Releases](../../releases), open it, and drag **Printables Offline.app**
to your Applications folder.

Because the app is open source and not signed with a paid Apple Developer account, macOS Gatekeeper
blocks the first launch:

1. Try to open **Printables Offline.app** and let Gatekeeper block it.
2. Open **System Settings → Privacy & Security**.
3. Click **Open Anyway** next to the Printables Offline notice.
4. Confirm **Open**.

You only do this once.

## First run

1. **Preferences → Storage → Library folder** — pick where models should live. The app creates a
   `Printables Offline Library` folder inside your choice.
2. **Preferences → Slicer** — pick PrusaSlicer, OrcaSlicer, Bambu Studio or UltiMaker Cura.
3. **Sign in** from the sidebar account strip (or **My Models**).
4. **My Models → Sync all models**.

## How sync works

Pulling is deliberately conservative:

- **Idempotent.** A file is skipped when it already exists at the size Printables reports.
- **Resumable.** Downloads land in `<name>.part` and are renamed only on success, so an interrupted
  sync never leaves a truncated model file behind.
- **No duplicates.** A model is matched by its numeric id, so a URL with a different slug will not
  create a second folder.
- **Your edits survive.** If `metadata.json` has uncommitted local changes, a pull updates stats and
  files but keeps your name/description/tags.
- **Nothing is silent.** Files that fail to download are reported as warnings on the job, not
  swallowed.
- **Bounded concurrency.** 1–8 models in parallel (default 3), with a global ~4 requests/second
  budget and `Retry-After` handling.

## How local git works

Each model folder gets a `.git` repository containing `metadata.json`, `files/` and `images/`.

- Cloning or first sync: `Imported from printables.com`
- A later pull: `Pulled changes from printables.com` (only when something actually changed)
- Edits on disk: `Local edits`, committed automatically after a 5-second quiet period
- A successful push: `Synced changes to printables.com`
- A restore: `Restored revision <sha>`, so the restore itself can be undone

Repositories are ordinary `.git` directories — `git log`, `git diff` and any GUI client work on them.
History is kept per model, and **Preferences → Version control** lets you commit manually, restore a
revision, or compact a long history (a `pre-compact-*` tag preserves the old commits).

Model files are committed as-is. Model files are large, so a folder edited many times grows
accordingly; use **Compact history** on a model that has accumulated a long binary history.

## Pushing changes back

**Preferences → Version control → Allow pushing to printables.com** is off by default. With it on,
a model's page offers **Push changes…**, which:

1. compares the folder with printables.com right now,
2. lists every operation (upload, delete, metadata update) with details,
3. refuses to run if the website has files this folder has never seen (pull first),
4. writes a pre-push commit so the push is revertable,
5. journals every attempt to `<library>/.manager/push-log.jsonl`.

> **Status of the write API.** Printables publishes no documentation for writing models, so the app
> discovers what exists instead of guessing: **Probe write API** introspects the GraphQL schema and
> records the mutations it finds. Pushes only run for capabilities that were actually discovered, and
> refuse with a precise message when one is missing. If your account's API does not expose those
> mutations, pushes stay unavailable and nothing is sent — pulls, git and local editing are unaffected.

Creating brand-new models on printables.com is out of scope. Collections stay local; they are not
synced to the website.

## Where your data lives

| What | Where |
|---|---|
| Preferences | `~/Library/Application Support/dev.printablesoffline/prefs.json` |
| Session | your login Keychain (keychain item `dev.printablesoffline.app`) |
| Library | your chosen folder, e.g. `…/Printables Offline Library/<id>-<slug>/` |
| Collections | `<library>/collections.json` |
| Bookkeeping | `<library>/.manager/` (sync state, capability report, push journal) |

A 1.x `~/.printablesoffline/prefs.json` is migrated automatically on first launch and left behind as
`prefs.json.migrated`.

## Credentials

The app never sees or stores your printables.com password. You sign in on the real website in its own
window, so Google/Apple SSO, 2FA and the password you already have in iCloud Keychain all behave
exactly as they do in Safari. What the app keeps is the resulting session token, in your **login
Keychain**:

- **Preferences → Account & credentials → Session storage** shows which store is in use, and
  **Test Keychain** performs a real write/read/delete round-trip so you can see it work.
- If macOS refuses the Keychain write (for example because an ad-hoc-signed build changed identity),
  the token is kept in a `0600` file and the app *says so* — it never falls back silently. **Store in
  Keychain** retries once you have approved the access prompt.
- The refresh token is kept alongside the access token, so an expired session is renewed silently on
  launch instead of asking you to sign in again.

Because this build is signed ad-hoc rather than with a paid Apple Developer identity, macOS may show
an "allow access to your keychain" prompt on the first write; choosing **Always Allow** keeps it away.
Inline iCloud Keychain *autofill* inside a third-party webview is only available to apps with an
associated-domains entitlement, so treat the sign-in window as a normal website login rather than an
autofilling form.


## Build from source

```bash
git clone https://github.com/jklock/PrintablesManager.git
cd PrintablesManager/src-tauri
cargo tauri build          # -> target/release/bundle/dmg/
cargo tauri dev            # hot-reload development build
```

The UI is plain ES2020 in `ui/dist/` — no bundler, no `npm install`, no build step. Edit the files and
reload.

Tests:

```bash
cd src-tauri
cargo test                 # offline unit tests
PO_LIVE=1 cargo test -- --ignored   # tests that hit printables.com
```

## Architecture

```
src-tauri/src/
  api/…        printables GraphQL client: bearer auth, errors[], rate limit, atomic downloads
  auth/        Keychain session + the in-app login window
  config.rs    App Support paths, prefs v2, 1.x migration
  error.rs     coded AppError (NOT_AUTHENTICATED, RATE_LIMITED, CONFLICT, …)
  git/         per-model repositories: commit, status, log, diff, restore, compact
  jobs.rs      job registry: list, progress, cancel
  library/     indexer (ModelSummary + git state) and the one model-dir locator
  printables/  pull pipeline (idempotent, resumable) and URL parsing
  sync/        sync engine, watcher, push planner, capability probe, push executor
  commands/    thin Tauri command layer
ui/dist/       index.html, app.js, app.css (no build step)
```

## Platform support

macOS 12+ only. Windows support was removed in 2.0 along with the Windows installer, the Windows
icon, the `USERPROFILE` fallbacks and the cross-compilation instructions. Linux bundles were dropped
at the same time; the app ships a `.dmg`.

## Releases

| Version | Date | Notes |
|---------|------|-------|
| 2.0.0 | 2026-11-19 | macOS only. Account login, sync-all of authored models, per-model git history, local editing with reviewed push, preferences moved to Application Support. Windows and Linux packaging removed. |
| 1.2.0 | 2026-11-05 | Pure-Rust Printables GraphQL and media client; removed the Python runtime. |
| 1.1.0 | 2026-09-13 | Collections; Windows x64/ARM64 builds. |
| 1.0.1 | 2026-09-10 | Cover images use the first downloaded picture. |
| 1.0.0 | 2026-08-24 | Initial release. |

## Credits

Originally forked from [JacRob32/Printables-Offline](https://github.com/JacRob32/Printables-Offline).
Printables and the Printables logo are trademarks of Prusa Research. This is an unofficial client and
is not affiliated with or endorsed by Prusa Research.

## License

See [LICENSE](LICENSE).
