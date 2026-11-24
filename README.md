# hx-echo

[English](./README.md) | [简体中文](./README.zh_CN.md)

A lightweight, HTMX-driven comment system built in **Rust (WebAssembly)** for static sites on **Cloudflare Workers & D1**, featuring a **Telegram Bot bidirectional moderation & reply loop** (no web admin token forms).

## How It Works (Base + Delta)

`hx-echo` splits comments into a **static baseline** and an **incremental runtime delta**:

1. **Build-Time Baseline (Edge Cached)**: During static site generation (e.g., Hugo `resources.GetRemote`), your SSG fetches `GET /api/comments/export?slug=/posts/hello-world/&lang=en` and bakes existing comments directly into the HTML along with `lastCursor`. Responses include `bakedAt` and `Cache-Control: public, max-age=60` (backed by Cloudflare's Cache API) to absorb bursty SSG rebuilds without hammering D1—no active cache purge is needed on new comments because the runtime delta endpoint fills in everything newer than `lastCursor`.
2. **Runtime Delta (HTMX)**: When the comment list enters the viewport (`hx-trigger="intersect once"`) or loads, HTMX requests `GET /api/comments?slug=/posts/hello-world/&after=<lastCursor>`.
   - If no new comments exist, the Worker returns `204 No Content` with `Cache-Control: no-store` (zero DOM changes).
   - New top-level comments are appended to `#comment-list`; replies to already-rendered static comments use `hx-swap-oob="beforeend:#replies-for-<parent_id>"` along with OOB `#reply-count-<parent_id>` updates.
3. **Comment Submission & Edge Protection**: Submitting the form via `hx-post="/api/comments"` runs optional per-IP rate limiting (`COMMENT_RATE_LIMITER`, returning `429` plain text), checks the hidden `website` honeypot field, blocks reserved `ADMIN_NAMES` (`403` plain text), renders Markdown (`pulldown-cmark`), sanitizes HTML (`ammonia`), saves to D1, dispatches an async Telegram notification via `ctx.wait_until(...)`, and returns the rendered `<li>` (using `HX-Retarget` / `HX-Reswap` headers and OOB reply count for replies).
4. **Telegram Bidirectional Control**: Admins reply directly to Telegram notifications to publish admin replies (`is_admin = 1`, marked with `.badge-admin`), `/hide` or `/del` spam, `/show` hidden comments, or run `/post <slug> <content>` to create top-level admin comments.
5. **Same-Origin Multi-Domain & Local Dev Support**: Mount the Worker under `<domain>/api/comments*` across mirror domains (`koa.im`, `www.koa.im`, etc.) to avoid CORS preflights. Loopback dev origins (`http://localhost:*`, `http://127.0.0.1:*`) and `CORS_ALLOW_ORIGINS` are supported automatically with full `HX-*` header exposure. Slugs are normalized to pathname-only (`RelPermalink`).

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the full specification and Hugo partial reference.

## Quick Start

### Option A: One-Click Guided Deploy via GitHub Actions

1. Fork this repository.
2. In GitHub **Settings → Secrets and variables → Actions → Secrets**, add:
   - Required: `CLOUDFLARE_API_TOKEN` (with Workers & D1 edit permissions) and `CLOUDFLARE_ACCOUNT_ID`.
   - Optional (for Telegram): `TELEGRAM_BOT_TOKEN`, `TELEGRAM_WEBHOOK_SECRET`, and optionally `TELEGRAM_CHAT_ID`.
3. Open the **Actions** tab → **Deploy hx-echo** → **Run workflow**, and fill in your required settings (`worker_name`, `domains`, `api_path`, `d1_database_name` / optional `d1_database_id`) and optional settings (`default_lang`, `site_origin`, `admin_names`, `admin_display_name`, `telegram_chat_id`, `enable_rate_limit`, `export_cache_max_age`).
   - The workflow automatically resolves or creates the D1 database, generates `wrangler.toml` on the runner, applies `migrations/` in order, syncs Secrets, deploys the Worker, and optionally calls Telegram `setWebhook`.
   - For automatic redeploys on future `push` events, either store your generated config in a `WRANGLER_TOML` Secret or set `HX_ECHO_*` Repository Variables (`HX_ECHO_DOMAINS`, `HX_ECHO_D1_DATABASE_ID`, etc.).

### Option B: Local CLI Setup & Manual Deploy

#### 1. Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) with the WebAssembly target:
  ```sh
  rustup target add wasm32-unknown-unknown
  ```
- [Node.js](https://nodejs.org/) (for `npx wrangler`, `>= 4.36.0` if using `[[ratelimits]]`) and a Cloudflare account

#### 2. Configure Wrangler & D1 Migrations

Create your local `wrangler.toml` from the template (`wrangler.toml` is gitignored so your personal database ID and domains stay private):

```sh
cp wrangler.toml.example wrangler.toml
```

Create a Cloudflare D1 database:

```sh
npx wrangler d1 create hx-echo-db
```

Update `wrangler.toml` with your `database_id` and domain `routes`, then apply the versioned migrations in `migrations/` (`0001_init.sql` → `0002_tg_admin.sql`):

```sh
# Local development database
npx wrangler d1 migrations apply DB --local

# Remote production database (works for both fresh installs and existing 0001 databases)
npx wrangler d1 migrations apply DB --remote
```

*(Note: `schema.sql` is kept as the latest full schema snapshot for reference.)*

#### 3. Configure Telegram Bot & Secrets (Optional)

Set sensitive credentials via Wrangler Secrets (never commit tokens to `wrangler.toml`):

```sh
npx wrangler secret put TELEGRAM_BOT_TOKEN
npx wrangler secret put TELEGRAM_WEBHOOK_SECRET
```

Then register your Telegram Webhook with a single `curl` command (replace `<BOT_TOKEN>`, `<YOUR_DOMAIN>`, and `<WEBHOOK_SECRET>`):

```sh
curl -X POST "https://api.telegram.org/bot<BOT_TOKEN>/setWebhook" -H "Content-Type: application/json" -d '{"url":"https://<YOUR_DOMAIN>/api/comments/tg-webhook","secret_token":"<WEBHOOK_SECRET>"}'
```

#### 4. Run Locally & Deploy

```sh
# Run unit tests
cargo test

# Start local Worker dev server (http://127.0.0.1:8787)
npx wrangler dev

# Build and deploy to Cloudflare Workers
npx wrangler deploy
```

## Optional Edge Rate Limiting

To protect D1 write quotas on `POST /api/comments`, uncomment the `[[ratelimits]]` block in `wrangler.toml`:

```toml
[[ratelimits]]
name = "COMMENT_RATE_LIMITER"
namespace_id = "1001"

  [ratelimits.simple]
  limit = 5
  period = 60
```

- **Optional by design**: If `[[ratelimits]]` is omitted (or `CF-Connecting-IP` is absent during local `wrangler dev`), `hx-echo` skips rate limiting without errors.
- **Constraints**: Cloudflare Workers Rate Limiting requires Wrangler `>= 4.36.0`, and `period` must be either `10` or `60` seconds. Triggered requests receive `429 Too Many Requests` (`Retry-After: 60`) with an i18n plain-text message.
- Telegram webhook calls (`POST /api/comments/tg-webhook`) bypass visitor IP rate limiting.

## API Endpoints

| Method | Path | Params / Auth | Response |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/&limit=20&max_bytes=32768&lang=en` | `200 OK` JSON `{ "lastCursor", "firstCursor", "hasMore", "exportedCount", "htmlBytes", "bakedAt", "html" }` (`Cache-Control: public, max-age=60`) |
| `GET` | `/api/comments` | `?slug=...&after=<id>` (delta) / `&before=<id>&limit=20` (older roots) / `&parent_id=<id>&before\|after=<id>` (thread replies), optional `&lang=` | `204 No Content` (idle delta) or `200 OK` `text/html` fragments (`Cache-Control: no-store`) |
| `POST` | `/api/comments` | Form: `slug`, `author`, `content`, `parent_id?`, `reply_to_author?`, `lang?`, `website` | `200 OK` `text/html` `<li>` (`403` reserved admin name, `429` rate limited) |
| `POST` | `/api/comments/tg-webhook` | Header: `X-Telegram-Bot-Api-Secret-Token`; JSON `Update` from `TELEGRAM_CHAT_ID` | `200 OK` (handles reply-to-publish, `/hide`, `/del`, `/show`, `/post <slug> <content>`) |

### Configuration Variables (`[vars]` & Secrets)

- `DEFAULT_LANG` (default `"en"`): fallback UI language (`en`, `zh-cn`, `zh-tw`, `ja`).
- `EXPORT_DEFAULT_LIMIT` (default `"20"`), `EXPORT_MAX_LIMIT` (default `"100"`), `EXPORT_MAX_BYTES` (default `"32768"`).
- `EXPORT_CACHE_MAX_AGE` (default `"60"`): `Cache-Control` `max-age` in seconds for `/api/comments/export` (`"0"` disables edge caching).
- `ADMIN_NAMES`: comma-separated reserved author names blocked on public web submissions (`403`).
- `ADMIN_DISPLAY_NAME` (default `"Admin"`): display name for Telegram-originated admin comments (automatically treated as reserved too).
- `SITE_ORIGIN`: canonical site origin (e.g. `https://example.com`) for clickable article links in Telegram notifications.
- `TELEGRAM_CHAT_ID`: authorized numeric Telegram chat ID (can be a `[vars]` entry or Secret).
- `TELEGRAM_BOT_TOKEN` & `TELEGRAM_WEBHOOK_SECRET`: Worker Secrets set via `wrangler secret put`.

### Internationalization (i18n)

`hx-echo` supports both **server-rendered localization** and **client-side custom overrides**:
- **Server-side locales**: Built-in translations for `en` (default), `zh-cn`, `zh-tw`, and `ja`, resolved in priority order: `lang` (form/query) > `HX-Lang` header > `Accept-Language` header > `DEFAULT_LANG` env var > `en`. Explicit `?lang=` is preserved in pagination button URLs.
- **Client-side overrides**: Rendered elements carry semantic attributes (`data-i18n="admin-badge"`, `data-i18n="reply"`, `data-i18n="reply-count"` with `data-count`, `data-i18n="load-older-comments"`, `data-i18n="load-older-replies-remaining"` with `data-remaining`, `data-i18n="load-replies-total"` with `data-total`, `data-i18n="load-older-replies"`, `data-i18n="load-newer-replies"`).

## Frontend Integration (Hugo + HTMX)

1. Include `htmx.min.js` in your static site.
2. Add the Hugo comment partial from [ARCHITECTURE.md](./ARCHITECTURE.md#6-hugo-partial-reference-layoutspartialscommentshtml), setting `site.Params.hxEchoExportBase` to your deployed `/api/comments` export base URL for build-time fetching.

## License

[MIT](./LICENSE)
