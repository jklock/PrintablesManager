# Printables API notes (2.0)

This file records what the app actually relies on, what is verified, and what still needs
confirmation against a live account. It exists so that nobody has to guess later.

## Verified against the public API

These operations are exercised by the ignored live tests (`PO_LIVE=1 cargo test -- --ignored`) and by
normal cloning:

| Operation | Document | Notes |
|---|---|---|
| Model metadata | `query ModelTags($id: ID!) { print(id:) { … } }` | name, slug, summary, description, rating, counts, author, tags, cover |
| File list | `query ModelFiles($id: ID!) { print(id:) { gcodes stls slas otherFiles } }` | includes `folder`, `note`, `fileSize`, `filePreviewPath` |
| Download ticket | `mutation GetDownloadLink(...)` | `source: "model_detail"`, returns `{ ok, errors, output { link, ttl } }` |
| Gallery | `query ModelImages($id: ID!)` | `images[].filePath` → `https://media.printables.com/…` |

Anonymous access works for all of the above. Requests are spaced ~250 ms apart; `429` is retried twice
honouring `Retry-After`.

## Assumed, not yet verified

The account-specific pieces cannot be verified without a real session:

| Assumption | Where | Failure mode if wrong |
|---|---|---|
| Auth0 SPA caches the access token in `localStorage` under a key containing `auth` | `auth/login.rs` | The login window stays open and no token is captured |
| `api.printables.com/graphql/` accepts `Authorization: Bearer <token>` | `printables/api.rs` | `401`/`403` → `NOT_AUTHENTICATED` |
| `query Viewer { viewer { id publicUsername } }` exists | `printables/api.rs::viewer` | Sign-in reports "did not return an account" |
| `searchPrints2(query:, limit:, cursor:)` can list an author's models | `printables/api.rs::author_models` | `account_models` errors; nothing is downloaded |
| `cursor` is returned under `data.result.cursor` | same | Sync falls back to a single page of 100 |

Each of these fails loudly with a coded error rather than silently producing an empty library.

## Write API

There is **no public documentation** for writing to printables.com. The app therefore does not
hard-code mutation names. `sync/capabilities.rs` introspects `__type(name: "Mutation") { fields { name
args { name type { … } } } }` and stores the result in `<library>/.manager/capabilities.json`.

Roles are matched heuristically:

| Role | Match |
|---|---|
| `add_file` | name contains `file` and one of add/create/upload/attach |
| `delete_file` | name contains `file` and one of delete/remove |
| `update_model` | name contains `print` and one of update/edit/patch |
| `add_image` | name contains `image` and one of add/create/upload |

`sync/push.rs` builds the mutation document from the discovered argument types and refuses to run when
a required role or argument cannot be satisfied. It never falls back to a guessed document.

**If introspection is disabled**, the probe reports `UNSUPPORTED` and pushes remain unavailable. In that
case the remaining option is capturing the web app's own requests with a proxy and adding explicit
operation documents — a deliberate, reviewed change, not a heuristic.

## Safety rules encoded in the client

- A push is refused when the website has file ids the folder has never seen (`remote_changed`).
- A push is refused when the plan the user reviewed no longer matches (`plan_id` mismatch).
- Writes require `remote_write_enabled` in preferences, which is off by default.
- Every attempt is appended to `.manager/push-log.jsonl`.
- A commit is created before pushing and after a fully successful push.

## Rate limiting / etiquette

- One shared budget of ~4 GraphQL requests/second per process.
- Downloads are sequential per model; at most `sync_concurrency` (1–8) models run in parallel.
- No scraping of HTML pages; only the GraphQL endpoint and the media CDN.
