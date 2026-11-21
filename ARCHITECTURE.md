# `hx-echo` Architecture & Implementation Specification

> **Project Overview**: `hx-echo` is a zero-overhead, HTMX-driven comment system built in **Rust (WebAssembly)** for static sites (**Hugo + Cloudflare Workers & D1**).
> **License**: MIT
> **Core Paradigm**: **Static Baseline + Dynamic Delta (Base + Delta)** — Historical comments are baked directly into static HTML at build time (`0ms` TTFB, 100% SEO-friendly), while new comments after build are fetched incrementally via HTMX (`204 No Content` when idle).

---

## 1. System Architecture & Data Flow

### 1.1 Build-Time (Hugo SSG)
1. During `hugo --minify`, Hugo invokes `resources.GetRemote` targeting `GET /api/comments/export?slug={RelPermalink}`.
2. The Rust Worker queries D1 for all approved comments (`status = 1`) for the normalized `slug`, constructs the two-level nested HTML tree, and returns JSON containing `{ "lastCursor": <max_id>, "html": "<rendered_li_elements>" }`.
3. Hugo embeds the rendered HTML directly into the static page and sets `after={{ $lastCursor }}` on the HTMX container.
4. **Graceful Degradation**: Wrapped in `try (...)` so network hiccups or initial deployment cold starts fall back safely to `lastCursor = 0` and empty HTML without breaking CI/CD builds.

### 1.2 Runtime Diff Loading (HTMX)
1. On page load (`hx-trigger="load"`), HTMX sends `GET /api/comments?slug={slug}&after={lastCursor}`.
2. **Zero-Cost Idle**: If no comments exist with `id > after`, the Worker returns `204 No Content` with an empty body. HTMX performs zero DOM mutations.
3. **Incremental Append & OOB Swaps**:
   - New **top-level comments** are returned as standard `<li>` elements and appended to `#comment-list` (`hx-swap="beforeend"`).
   - New **child replies** (replies to existing static comments) are rendered with `hx-swap-oob="beforeend:#replies-for-{parent_id}"`, allowing HTMX to out-of-band inject them directly into the pre-rendered parent thread.

### 1.3 Comment Submission (Mutation)
1. Single reusable `<form>` submits via `hx-post="/api/comments"`.
2. **Honeypot Check**: Hidden field `website` must be empty; otherwise return `204 No Content` silently.
3. **Markdown & XSS Sanitization**: Raw input is parsed via `pulldown-cmark` and sanitized via `ammonia` on the edge before D1 insertion.
4. **Dynamic Retargeting**:
   - Top-level comments return the rendered `<li>` and append to `#comment-list`.
   - Child replies include response headers `HX-Retarget: #replies-for-{parent_id}` and `HX-Reswap: beforeend` to override the form's default target and insert directly into the parent's reply container.

### 1.4 Multi-Domain Same-Origin Routing
- Instead of hosting on a separate API subdomain (which triggers CORS preflights on HTMX custom headers like `HX-Request`), the Rust Worker is mounted via **Cloudflare Custom Routes** on `*<domain>/api/comments*` across all mirror domains.
- All frontend requests use relative paths (`/api/comments`), making every request strictly **Same-Origin**.
- Comment isolation/sharing across mirror domains is guaranteed by normalizing the article identifier to `RelPermalink` (pathname only, stripping protocol and host).

---

## 2. Repository Directory Structure

```text
hx-echo/
├── Cargo.toml                  # Rust dependencies & Wasm release size optimizations
├── wrangler.toml               # Cloudflare Worker, D1 binding, and multi-domain routes
├── schema.sql                  # D1 SQLite schema & indexes
├── LICENSE                     # MIT License
├── README.md                   # Project documentation & self-hosting guide
├── ARCHITECTURE.md             # This file (detailed architecture & implementation spec)
├── src/
│   └── lib.rs                  # Rust Worker entrypoint, router, D1 queries, HTML renderer
└── hugo-example/
    ├── layouts/
    │   └── partials/
    │       └── comments.html   # Hugo partial integrating static build + HTMX
    └── static/
        └── js/
            └── htmx.min.js     # Self-hosted HTMX (~14KB gzipped)
```

---

## 3. Database Schema (`schema.sql`)

Flat storage with two-level tree normalization (`parent_id` always points to the root comment ID; nested replies track `@target` via `reply_to_author`).

```sql
CREATE TABLE IF NOT EXISTS comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  slug TEXT NOT NULL,                 -- Normalized pathname, e.g. "/posts/hello-world/"
  parent_id INTEGER DEFAULT NULL,     -- NULL = top-level comment; integer = root comment id
  reply_to_author TEXT DEFAULT NULL,  -- Target username for @mention display in nested threads
  author TEXT NOT NULL,
  content_html TEXT NOT NULL,         -- Sanitized HTML rendered from Markdown
  created_at INTEGER NOT NULL,        -- Unix timestamp (seconds)
  status INTEGER DEFAULT 1            -- 1 = published, 0 = pending/hidden
);

CREATE INDEX IF NOT EXISTS idx_comments_slug_id ON comments(slug, status, id);
```

---

## 4. Rust Worker Configuration (`Cargo.toml` & `wrangler.toml`)

### `Cargo.toml`
```toml
[package]
name = "hx-echo"
version = "0.1.0"
edition = "2021"
license = "MIT"

[lib]
crate-type = ["cdylib"]

[dependencies]
worker = { version = "0.4", features = ["d1"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
pulldown-cmark = "0.12"
ammonia = "4.0"

[profile.release]
opt-level = "s"     # Optimize for binary size (.wasm)
lto = true          # Link-Time Optimization
strip = true        # Strip debug symbols
codegen-units = 1
```

### `wrangler.toml`
```toml
name = "hx-echo"
main = "build/worker/shim.mjs"
compatibility_date = "2025-01-01"

[build]
command = "cargo install -q worker-build && worker-build --release"

[[d1_databases]]
binding = "DB"
database_name = "hx-echo-db"
database_id = "REPLACE_WITH_YOUR_D1_DATABASE_ID"

# Mount the same Worker across all mirror domains under /api/comments*
routes = [
  { pattern = "example.com/api/comments*", zone_name = "example.com" },
  { pattern = "mirror.example.org/api/comments*", zone_name = "mirror.example.org" }
]
```

---

## 5. API Contract & HTML Fragment Specification

### 5.1 Unified HTML Fragment Structure
Both build-time export and runtime diff/post endpoints **must** emit identical DOM structures:

```html
<!-- Top-level comment -->
<li id="comment-101" class="comment-item">
  <div class="comment-meta">
    <strong>Alice</strong>
    <time data-ts="1732123456">1732123456</time>
    <button type="button" class="reply-btn" onclick="setReplyTarget(101, 'Alice')">回复</button>
  </div>
  <div class="comment-body"><p>Hello world!</p></div>
  <ul id="replies-for-101" class="comment-replies">
    <!-- Child replies live here -->
    <li id="comment-102" class="comment-reply-item">
      <div class="comment-meta">
        <strong>Bob</strong>
        <time data-ts="1732123500">1732123500</time>
        <button type="button" class="reply-btn" onclick="setReplyTarget(101, 'Bob')">回复</button>
      </div>
      <div class="comment-body"><span class="reply-to">@Alice</span> <p>Welcome!</p></div>
    </li>
  </ul>
</li>
```

### 5.2 Endpoints Summary

| Method | Path | Query / Body Params | Response |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/` | `200 OK` JSON: `{ "lastCursor": i64, "html": String }` |
| `GET` | `/api/comments` | `?slug=/posts/xxx/&after=101` | `204 No Content` (if empty) or `200 OK` `text/html` (child items carry `hx-swap-oob="beforeend:#replies-for-{parent_id}"`) |
| `POST` | `/api/comments` | Form: `slug`, `author`, `content`, `parent_id?`, `reply_to_author?`, `website` (honeypot) | `200 OK` `text/html` single `<li>` item. Sets `HX-Retarget: #replies-for-{parent_id}` & `HX-Reswap: beforeend` if `parent_id` is present |

---

## 6. Hugo Partial Reference (`layouts/partials/comments.html`)

```html
{{ $slug := .RelPermalink }}
{{ $apiBase := site.Params.hxEchoExportBase | default "https://example.com/api/comments" }}
{{ $staticHtml := "" }}
{{ $lastCursor := 0 }}

{{ $exportUrl := printf "%s/export?slug=%s" $apiBase ($slug | urlquery) }}
{{ with try (resources.GetRemote $exportUrl) }}
  {{ with .Value }}
    {{ $data := .Content | transform.Unmarshal }}
    {{ $staticHtml = $data.html }}
    {{ $lastCursor = $data.lastCursor }}
  {{ end }}
{{ end }}

<section id="comments-section">
  <h3>评论区</h3>

  <ul id="comment-list"
      hx-get="/api/comments?slug={{ $slug | urlquery }}&after={{ $lastCursor }}"
      hx-trigger="load"
      hx-swap="beforeend">
    {{ $staticHtml | safeHTML }}
  </ul>

  <form id="comment-form"
        hx-post="/api/comments"
        hx-target="#comment-list"
        hx-swap="beforeend"
        hx-on::after-request="if(event.detail.successful) { this.reset(); clearReplyTarget(); }">
    
    <input type="hidden" name="slug" value="{{ $slug }}">
    <input type="hidden" id="parent_id" name="parent_id" value="">
    <input type="hidden" id="reply_to_author" name="reply_to_author" value="">

    <div id="reply-indicator" style="display:none;">
      正在回复 <strong id="reply-target-name"></strong>
      <button type="button" onclick="clearReplyTarget()">取消回复</button>
    </div>

    <!-- Honeypot anti-spam -->
    <div style="position:absolute; left:-9999px;" aria-hidden="true">
      <input type="text" name="website" tabindex="-1" autocomplete="off">
    </div>

    <div>
      <input type="text" name="author" placeholder="你的昵称" required maxlength="30">
    </div>
    <div>
      <textarea id="comment-textarea" name="content" placeholder="写下你的想法（支持 Markdown）..." required maxlength="2000"></textarea>
    </div>
    <button type="submit">发表评论</button>
  </form>
</section>

<script>
  function setReplyTarget(rootId, authorName) {
    document.getElementById('parent_id').value = rootId;
    document.getElementById('reply_to_author').value = authorName;
    document.getElementById('reply-target-name').textContent = '@' + authorName;
    document.getElementById('reply-indicator').style.display = 'block';
    document.getElementById('comment-textarea').focus();
  }

  function clearReplyTarget() {
    document.getElementById('parent_id').value = '';
    document.getElementById('reply_to_author').value = '';
    document.getElementById('reply-indicator').style.display = 'none';
  }
</script>
<script src="/js/htmx.min.js" defer></script>
```

---

## 7. Next Implementation Milestones (For AI Agent Context)

1. **Implement `src/lib.rs`**:
   - Add `/api/comments/export` handler using an in-memory tree builder (`build_comment_tree_html`) to group flat D1 rows into root + children HTML.
   - Implement `/api/comments` (`GET` diff & `POST` mutation) with `pulldown-cmark` + `ammonia`.
   - Ensure strict HTML attribute escaping for `onclick="setReplyTarget(...)"` to prevent single-quote injection in author names.
2. **Admin & Moderation Extension (Future)**:
   - Add `ADMIN_TOKEN` secret in Worker env for comment deletion/hiding (`status = 0`).
   - Optional debounced GitHub `repository_dispatch` webhook via Cloudflare KV/Queues to trigger Hugo static rebuilds after new comments settle.
3. **One-Click Self-Hosting**:
   - Provide a GitHub Actions workflow (`deploy.yml`) running `npx wrangler deploy` so forks can deploy without installing Rust locally.