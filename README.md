# hx-echo

[English](./README.md) | [简体中文](./README.zh_CN.md)

A lightweight, HTMX-driven comment system built in **Rust (WebAssembly)** for static sites on **Cloudflare Workers & D1**.

## How It Works (Base + Delta)

`hx-echo` splits comments into a **static baseline** and an **incremental runtime delta**:

1. **Build-Time Baseline**: During static site generation (e.g., Hugo `resources.GetRemote`), your SSG fetches `GET /api/comments/export?slug=/posts/hello-world/&lang=en` and bakes existing comments directly into the HTML along with `lastCursor`.
2. **Runtime Delta (HTMX)**: When the comment list enters the viewport (`hx-trigger="intersect once"`) or loads, HTMX requests `GET /api/comments?slug=/posts/hello-world/&after=<lastCursor>`.
   - If no new comments exist, the Worker returns `204 No Content` with `Cache-Control: no-store` (zero DOM changes).
   - New top-level comments are appended to `#comment-list`; replies to already-rendered static comments use `hx-swap-oob="beforeend:#replies-for-<parent_id>"` along with OOB `#reply-count-<parent_id>` updates.
3. **Comment Submission**: Submitting the form via `hx-post="/api/comments"` renders Markdown (`pulldown-cmark`), sanitizes HTML (`ammonia`), checks the hidden `website` honeypot field, saves to D1, and returns the rendered `<li>` (using `HX-Retarget` / `HX-Reswap` headers and OOB reply count for replies).
4. **Same-Origin Multi-Domain & Local Dev Support**: Mount the Worker under `<domain>/api/comments*` across mirror domains (`koa.im`, `www.koa.im`, etc.) to avoid CORS preflights. Loopback dev origins (`http://localhost:*`, `http://127.0.0.1:*`) and `CORS_ALLOW_ORIGINS` are supported automatically with full `HX-*` header exposure. Slugs are normalized to pathname-only (`RelPermalink`).

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the full specification and Hugo partial reference.

## Quick Start

### 1. Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) with the WebAssembly target:
  ```sh
  rustup target add wasm32-unknown-unknown
  ```
- [Node.js](https://nodejs.org/) (for `npx wrangler`) and a Cloudflare account

### 2. Configure Wrangler & D1

Create your local `wrangler.toml` from the template (`wrangler.toml` is gitignored so your personal database ID and domains stay private):

```sh
cp wrangler.toml.example wrangler.toml
```

Create a Cloudflare D1 database:

```sh
npx wrangler d1 create hx-echo-db
```

Update `wrangler.toml` with your `database_id` and domain `routes`, then initialize the schema:

```sh
# Local development database
npx wrangler d1 execute hx-echo-db --local --file=./schema.sql

# Remote production database
npx wrangler d1 execute hx-echo-db --remote --file=./schema.sql
```

### 3. Run Locally & Deploy

```sh
# Run unit tests
cargo test

# Start local Worker dev server (http://127.0.0.1:8787)
npx wrangler dev

# Build and deploy to Cloudflare Workers
npx wrangler deploy
```

## API Endpoints

| Method | Path | Params | Response |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/&limit=20&max_bytes=32768&lang=en` | `200 OK` JSON `{ "lastCursor": <id>, "firstCursor": <id>, "hasMore": bool, "exportedCount": n, "htmlBytes": n, "html": "..." }` |
| `GET` | `/api/comments` | `?slug=...&after=<id>` (delta) / `&before=<id>&limit=20` (older roots) / `&parent_id=<id>&before\|after=<id>` (thread replies), optional `&lang=` | `204 No Content` (idle delta) or `200 OK` `text/html` fragments (`Cache-Control: no-store`) |
| `POST` | `/api/comments` | Form: `slug`, `author`, `content`, `parent_id?`, `reply_to_author?`, `lang?`, `website` | `200 OK` `text/html` `<li>` (sets `HX-Retarget` / `HX-Reswap` and OOB reply count for replies) |

### Export Limits & Byte Circuit Breaker

In `wrangler.toml` (`[vars]`), administrators can configure:
- `DEFAULT_LANG` (default `"en"`): fallback UI language (`en`, `zh-cn`, `zh-tw`, `ja`) when `?lang=`, `HX-Lang`, or `Accept-Language` is not specified.
- `EXPORT_DEFAULT_LIMIT` (default `"20"`): default max comment items exported per page build.
- `EXPORT_MAX_LIMIT` (default `"100"`): upper bound for `?limit=`.
- `EXPORT_MAX_BYTES` (default `"32768"`, i.e. 32 KiB): default and forced maximum UTF-8 byte size of exported `html` (callers can request a smaller `?max_bytes=` / `?maxBytes=`, never larger).

When exported history or thread replies are truncated, `hx-echo` emits user-triggered HTMX buttons (`#load-older-comments`, `#load-older-replies-<id>`, `#load-newer-replies-<id>`) so visitors can load older comments and older/newer replies on demand.

### Internationalization (i18n)

`hx-echo` supports both **server-rendered localization** and **client-side custom overrides**:
- **Server-side locales**: Built-in translations for `en` (default), `zh-cn`, `zh-tw`, and `ja`, resolved in priority order: `lang` (form/query) > `HX-Lang` header > `Accept-Language` header > `DEFAULT_LANG` env var > `en`. Explicit `?lang=` is preserved in pagination button URLs.
- **Client-side overrides**: Rendered elements carry semantic attributes (`data-i18n="reply"`, `data-i18n="reply-count"` with `data-count`, `data-i18n="load-older-comments"`, `data-i18n="load-older-replies-remaining"` with `data-remaining`, `data-i18n="load-replies-total"` with `data-total`, `data-i18n="load-older-replies"`, `data-i18n="load-newer-replies"`) so static site templates can localize or customize labels to any language on `htmx:afterSettle`.

## Frontend Integration (Hugo + HTMX)

1. Include `htmx.min.js` in your static site.
2. Add the Hugo comment partial from [ARCHITECTURE.md](./ARCHITECTURE.md#6-hugo-partial-reference-layoutspartialscommentshtml), setting `site.Params.hxEchoExportBase` to your deployed `/api/comments` export base URL for build-time fetching.

## License

[MIT](./LICENSE)
