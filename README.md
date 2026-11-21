# hx-echo

A lightweight, HTMX-driven comment system built in **Rust (WebAssembly)** for static sites on **Cloudflare Workers & D1**.

## How It Works (Base + Delta)

`hx-echo` splits comments into a **static baseline** and an **incremental runtime delta**:

1. **Build-Time Baseline**: During static site generation (e.g., Hugo `resources.GetRemote`), your SSG fetches `GET /api/comments/export?slug=/posts/hello-world/` and bakes existing comments directly into the HTML along with `lastCursor`.
2. **Runtime Delta (HTMX)**: On page load (`hx-trigger="load"`), HTMX requests `GET /api/comments?slug=/posts/hello-world/&after=<lastCursor>`.
   - If no new comments exist, the Worker returns `204 No Content` (zero DOM changes).
   - New top-level comments are appended to `#comment-list`; replies to already-rendered static comments use `hx-swap-oob="beforeend:#replies-for-<parent_id>"`.
3. **Comment Submission**: Submitting the form via `hx-post="/api/comments"` renders Markdown (`pulldown-cmark`), sanitizes HTML (`ammonia`), checks the hidden `website` honeypot field, saves to D1, and returns the rendered `<li>` (using `HX-Retarget` / `HX-Reswap` headers for replies).
4. **Same-Origin Multi-Domain Support**: Mount the Worker under `*<domain>/api/comments*` across mirror domains to avoid CORS preflights. Slugs are normalized to pathname-only (`RelPermalink`).

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
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/` | `200 OK` JSON `{ "lastCursor": <id>, "html": "..." }` |
| `GET` | `/api/comments` | `?slug=/posts/xxx/&after=<id>` | `204 No Content` if idle, or `200 OK` `text/html` fragments |
| `POST` | `/api/comments` | Form: `slug`, `author`, `content`, `parent_id?`, `reply_to_author?`, `website` | `200 OK` `text/html` `<li>` (sets `HX-Retarget` / `HX-Reswap` for replies) |

## Frontend Integration (Hugo + HTMX)

1. Include `htmx.min.js` in your static site.
2. Add the Hugo comment partial from [ARCHITECTURE.md](./ARCHITECTURE.md#6-hugo-partial-reference-layoutspartialscommentshtml), setting `site.Params.hxEchoExportBase` to your deployed `/api/comments` export base URL for build-time fetching.

## License

[MIT](./LICENSE)
