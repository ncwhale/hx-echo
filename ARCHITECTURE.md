# `hx-echo` Architecture & Implementation Specification

> **Project Overview**: `hx-echo` is a zero-overhead, HTMX-driven comment system built in **Rust (WebAssembly)** for static sites (**Hugo + Cloudflare Workers & D1**).
> **License**: MIT
> **Core Paradigm**: **Static Baseline + Dynamic Delta (Base + Delta)** — Historical comments are baked directly into static HTML at build time (`0ms` TTFB, 100% SEO-friendly), while comments posted after the last build are fetched incrementally via HTMX (`204 No Content` when idle).

---

## 1. System Architecture & Data Flow

### 1.1 Build-Time Baseline (Hugo SSG)
1. During `hugo --minify`, Hugo invokes `resources.GetRemote` targeting `GET /api/comments/export?slug={RelPermalink}&lang={lang}`.
2. The Rust Worker queries D1 for approved comments (`status = 1`) for the normalized `slug`, packs the newest root comments and up to `INITIAL_INLINE_REPLIES` (5) replies per root within both item limits (`EXPORT_DEFAULT_LIMIT` / `EXPORT_MAX_LIMIT`) and a forced UTF-8 byte circuit breaker (`EXPORT_MAX_BYTES`, default 32 KiB).
3. It returns JSON containing `{ "lastCursor": <max_id>, "firstCursor": <oldest_root_id>, "hasMore": bool, "exportedCount": n, "htmlBytes": n, "html": "..." }`.
4. Hugo embeds `html` directly into the static page and sets `after={{ $lastCursor }}` on `#comment-list`.
5. **Graceful Degradation**: Wrapped in `try (...)` so network hiccups, offline builds, or uninitialized deployments fall back safely to `lastCursor = 0` and empty HTML without breaking CI/CD builds.

### 1.2 Runtime Diff Loading & Lazy Trigger (HTMX)
1. When `#comment-list` enters the viewport (`hx-trigger="intersect once"`, or `load`), HTMX sends `GET /api/comments?slug={slug}&after={lastCursor}&lang={lang}`.
2. **Zero-Cost Idle**: If no comments exist with `id > after`, the Worker returns `204 No Content` with `Cache-Control: no-store`. HTMX performs zero DOM mutations.
3. **Incremental Append & OOB Swaps**:
   - New **top-level comments** are returned as standard `<li>` elements and appended to `#comment-list` (`hx-swap="beforeend"`).
   - New **child replies** (replies to existing static roots) carry `hx-swap-oob="beforeend:#replies-for-{parent_id}"` to inject directly into the parent's reply list, paired with an OOB `<span id="reply-count-{parent_id}" hx-swap-oob="outerHTML">` badge update for HTMX requests.
4. **Compatibility with `hx-boost` History Navigation**:
   - Sites using `hx-boost="true"` should isolate `#comments-section` with `hx-boost="false" hx-disinherit="*"` and `hx-push-url="false"` on `#comment-form`.
   - On `htmx:configRequest`, the client advances `after=` to the maximum rendered `#comment-<id>` in the DOM, and `htmx:beforeSwap` deduplicates incoming `li[id^="comment-"]` elements so Back/Forward history restoration never duplicates comments.

### 1.3 On-Demand Pagination (Byte/Item Truncation Recovery)
- When build-time export or thread replies are truncated, `hx-echo` emits HTMX trigger items (`#load-older-comments`, `#load-older-replies-<root_id>`, `#load-newer-replies-<root_id>`).
- Clicking these buttons hits `GET /api/comments?slug=...&before=<id>` or `GET /api/comments?slug=...&parent_id=<root_id>&before|after=<id>` and replaces the trigger item with `hx-swap="outerHTML"`.

### 1.4 Comment Submission (Mutation)
1. A single reusable `<form id="comment-form">` submits via `hx-post="/api/comments"`.
2. **Validation & Honeypot Check**: Hidden field `website` must be empty (otherwise returns `204 No Content` silently). `author` is limited to 50 characters and `content` to 2500 characters (`MAX_AUTHOR_CHARS`, `MAX_CONTENT_CHARS`).
3. **Markdown & XSS Sanitization**: Raw Markdown is parsed via `pulldown-cmark` (strikethrough, tables, tasklists enabled) and sanitized via `ammonia` on the edge before D1 insertion.
4. **Two-Level Tree Normalization & Dynamic Retargeting**:
   - `parent_id` is normalized on the server to always point to the root comment ID, while `reply_to_author` preserves `@target` mention context.
   - Top-level comments return the rendered `<li>` and append to `#comment-list`.
   - Child replies return response headers `HX-Retarget: #replies-for-{parent_id}` and `HX-Reswap: beforeend`, plus an OOB `#reply-count-{parent_id}` span update.

### 1.5 Multi-Domain Same-Origin Routing & Local Dev CORS
- **Production Same-Origin**: Mount the Worker via **Cloudflare Routes** on `<domain>/api/comments*` across all mirror domains (both apex and `www.`). Frontend requests use relative `/api/comments` with zero CORS preflights. All API responses set `Cache-Control: no-store`.
- **Same Identity Across Mirrors**: Article slugs are normalized to pathname-only (`RelPermalink`).
- **Local Dev CORS**: Requests from loopback origins (`http://localhost[:port]`, `http://127.0.0.1[:port]`, `http://[::1][:port]`) or origins listed in `CORS_ALLOW_ORIGINS` automatically receive `OPTIONS` preflight handling and `Access-Control-Allow-Origin` / `Access-Control-Expose-Headers` (exposing `HX-Retarget`, `HX-Reswap`, `HX-Reselect`, `HX-Trigger`, `HX-Location`, `HX-Push`, etc.).

### 1.6 Internationalization (i18n)
- **Built-in Server Locales**: `en` (default), `zh-cn`, `zh-tw`, `ja`, resolved in priority order: `lang` (form/query) > `HX-Lang` header > `Accept-Language` header (weighted by `q`) > `DEFAULT_LANG` env var > `en`. Explicit `?lang=` is preserved in pagination button URLs.
- **Client-Side Overrides**: Rendered controls and counters include semantic `data-i18n` attributes (`reply`, `reply-count` + `data-count`, `load-older-comments`, `load-older-replies`, `load-older-replies-remaining` + `data-remaining`, `load-replies-total` + `data-total`, `load-newer-replies`) so any static site generator can override labels using its own i18n dictionary.

---

## 2. Repository Directory Structure

```text
hx-echo/
├── Cargo.toml                  # Rust dependencies & Wasm release size optimizations
├── wrangler.toml.example       # Template for Cloudflare Worker, D1 binding, routes, and vars
├── schema.sql                  # D1 SQLite schema & indexes
├── LICENSE                     # MIT License
├── README.md                   # English documentation & self-hosting guide
├── README.zh_CN.md             # Simplified Chinese documentation
├── ARCHITECTURE.md             # This file (architecture & integration specification)
└── src/
    └── lib.rs                  # Rust Worker entrypoint, router, D1 queries, i18n, HTML renderer & unit tests
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
CREATE INDEX IF NOT EXISTS idx_comments_parent_id ON comments(parent_id, status, id);
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
authors = ["Whale Mo <1552933+ncwhale@users.noreply.github.com>"]

[lib]
crate-type = ["cdylib"]

[dependencies]
worker = { version = "0.9", features = ["d1"] }
worker-macros = { version = "0.9" }
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

### `wrangler.toml` (from `wrangler.toml.example`)
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
  { pattern = "www.example.com/api/comments*", zone_name = "example.com" },
  { pattern = "mirror.example.org/api/comments*", zone_name = "mirror.example.org" }
]

[vars]
DEFAULT_LANG = "en"
EXPORT_DEFAULT_LIMIT = "20"
EXPORT_MAX_LIMIT = "100"
EXPORT_MAX_BYTES = "32768"
```

> **Note on Wrangler Environments**: If you separate local dev (`hx-echo-dev`) and production (`[env.production]` with `hx-echo-prod`), remember that `routes`, `d1_databases`, and `vars` are **non-inheritable** in Wrangler and must each be declared under `[env.production]` as well.

---

## 5. API Contract & HTML Fragment Specification

### 5.1 Unified HTML Fragment Structure
Both build-time export and runtime diff/pagination/post endpoints emit identical DOM structures (with localized text matching the resolved `Locale` and `data-i18n` hooks):

```html
<!-- Optional pagination trigger when older root comments exist -->
<li id="load-older-comments" class="load-older-item">
  <button type="button" class="load-older-btn" data-i18n="load-older-comments"
          hx-get="/api/comments?slug=%2Fposts%2Fhello-world%2F&amp;before=101"
          hx-target="#load-older-comments" hx-swap="outerHTML">Load older comments</button>
</li>

<!-- Top-level comment -->
<li id="comment-101" class="comment-item" data-reply-count="1">
  <div class="comment-meta">
    <strong>Alice</strong>
    <time data-ts="1732123456">1732123456</time>
    <span id="reply-count-101" class="reply-count" data-count="1" data-i18n="reply-count">1 reply</span>
    <button type="button" class="reply-btn" data-i18n="reply" onclick="setReplyTarget(101, 'Alice')">Reply</button>
  </div>
  <div class="comment-body"><p>Hello world!</p></div>
  <ul id="replies-for-101" class="comment-replies">
    <!-- Optional pagination trigger when older replies in this thread are truncated -->
    <!-- <li id="load-older-replies-101" class="load-more-replies">...</li> -->

    <!-- Child reply -->
    <li id="comment-102" class="comment-reply-item">
      <div class="comment-meta">
        <strong>Bob</strong>
        <time data-ts="1732123500">1732123500</time>
        <button type="button" class="reply-btn" data-i18n="reply" onclick="setReplyTarget(101, 'Bob')">Reply</button>
      </div>
      <div class="comment-body"><span class="reply-to">@Alice</span> <p>Welcome!</p></div>
    </li>
  </ul>
</li>
```

### 5.2 Endpoints Summary

| Method | Path | Query / Body Params | Response |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/&limit=20&max_bytes=32768&lang=en` | `200 OK` JSON: `{ "lastCursor", "firstCursor", "hasMore", "exportedCount", "htmlBytes", "html" }` (`Cache-Control: no-store`) |
| `GET` | `/api/comments` | `?slug=...&after=<id>` (delta) / `&before=<id>&limit=20` (older roots) / `&parent_id=<id>&before\|after=<id>` (thread replies), optional `&lang=` | `204 No Content` (idle delta) or `200 OK` `text/html` fragments (OOB replies carry `hx-swap-oob="beforeend:#replies-for-{parent_id}"` + OOB `#reply-count-{parent_id}`) |
| `POST` | `/api/comments` | Form: `slug`, `author`, `content`, `parent_id?`, `reply_to_author?`, `lang?`, `website` (honeypot) | `200 OK` `text/html` single `<li>` item (sets `HX-Retarget` / `HX-Reswap` and OOB reply count if `parent_id` is present) |

---

## 6. Hugo Partial Reference (`layouts/partials/comments.html`)

In your `<head>`, load HTMX with `defer` (avoid `async`, which can race `DOMContentLoaded` in HTMX):

```html
{{- if hugo.IsServer }}
<meta name="htmx-config" content='{"selfRequestsOnly": false}'>
{{- end }}
<script defer src="{{ "js/htmx.min.js" | relURL }}"></script>
```

Reference `layouts/partials/comments.html` (supports Hugo `i18n`, `hugo.IsServer` local dev proxy, `hx-trigger="intersect once"`, `hx-boost` isolation & Back/Forward deduplication, timestamp formatting, and `data-i18n` overrides):

```html
{{- $commentsEnabled := ne (printf "%v" (index .Params "comments" | default true)) "false" -}}
{{- $hxEchoActive := or site.Params.hxEchoEnabled site.Params.hxEchoExportBase site.Params.hxEchoLocalBase -}}
{{- if and $commentsEnabled $hxEchoActive -}}
{{- $slug := .RelPermalink -}}
{{- $prodBase := site.Params.hxEchoExportBase | default "" -}}
{{- $localBase := site.Params.hxEchoLocalBase | default "http://127.0.0.1:8787/api/comments" -}}
{{- $exportBase := cond hugo.IsServer $localBase $prodBase -}}
{{- $staticHtml := "" -}}
{{- $lastCursor := 0 -}}
{{- $lang := site.Params.hxEchoLang | default (cond hugo.IsMultilingual site.Language.Lang (site.Language.Locale | default site.Language.Lang | default "en")) -}}

{{- if or (hasPrefix $exportBase "http://") (hasPrefix $exportBase "https://") -}}
  {{- $exportUrl := printf "%s/export?slug=%s&lang=%s" (strings.TrimRight "/" $exportBase) ($slug | urlquery) ($lang | urlquery) -}}
  {{- with try (resources.GetRemote $exportUrl) -}}
    {{- if not .Err -}}
      {{- with .Value -}}
        {{- $data := .Content | transform.Unmarshal -}}
        {{- with $data.html }}{{ $staticHtml = . }}{{ end -}}
        {{- with $data.lastCursor }}{{ $lastCursor = . }}{{ end -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
{{- end -}}

<section id="comments-section" class="comments hx-echo" hx-boost="false" hx-disinherit="*">
  <h3 class="hx-echo__title">{{ T "comments_title" }}</h3>

  {{- if hugo.IsServer }}
  {{- $parsed := urls.Parse $localBase }}
  {{- $localOrigin := printf "%s://%s" $parsed.Scheme $parsed.Host }}
  <script>
    (function () {
      if (window.__hxEchoLocalProxy) return;
      window.__hxEchoLocalProxy = true;
      var apiOrigin = {{ $localOrigin }};
      document.addEventListener('htmx:configRequest', function (evt) {
        var path = evt.detail && evt.detail.path;
        if (typeof path === 'string' && path.indexOf('/api/comments') === 0) {
          evt.detail.path = apiOrigin + path;
        }
      });
    })();
  </script>
  {{- end }}

  <ul id="comment-list"
      class="comment-list"
      data-empty-text="{{ T "comments_empty" }}"
      hx-get="/api/comments?slug={{ $slug | urlquery }}&after={{ $lastCursor }}&lang={{ $lang | urlquery }}"
      hx-trigger="intersect once"
      hx-swap="beforeend">{{ $staticHtml | safeHTML }}</ul>

  <form id="comment-form"
        class="comment-form"
        hx-post="/api/comments"
        hx-target="#comment-list"
        hx-swap="beforeend"
        hx-push-url="false"
        hx-disabled-elt="find button[type='submit']">
    <input type="hidden" name="slug" value="{{ $slug }}">
    <input type="hidden" name="lang" value="{{ $lang }}">
    <input type="hidden" id="parent_id" name="parent_id" value="">
    <input type="hidden" id="reply_to_author" name="reply_to_author" value="">

    <div id="reply-indicator" class="reply-indicator" style="display:none;">
      <span>{{ T "comments_replying_to" }} <strong id="reply-target-name"></strong></span>
      <button type="button" class="reply-cancel-btn" onclick="clearReplyTarget()">{{ T "comments_cancel_reply" }}</button>
    </div>

    <div style="position:absolute; left:-9999px;" aria-hidden="true">
      <input type="text" name="website" tabindex="-1" autocomplete="off">
    </div>

    <div class="comment-form__row">
      <input type="text" name="author" placeholder="{{ T "comments_author_placeholder" }}" required maxlength="50" autocomplete="name">
    </div>
    <div class="comment-form__row">
      <textarea id="comment-textarea" name="content" rows="4" placeholder="{{ T "comments_content_placeholder" }}" required maxlength="2500"></textarea>
    </div>
    <div id="comment-error" class="comment-error" role="alert" style="display:none;"></div>
    <div class="comment-form__actions">
      <button type="submit" class="comment-submit-btn">{{ T "comments_submit" }}</button>
    </div>
  </form>
</section>

<script>
  var HX_ECHO_I18N = {
    errorGeneric: {{ T "comments_error_generic" }},
    reply: {{ T "comments_reply" }},
    replyCountOne: {{ T "comments_reply_count" 1 }},
    replyCountOther: {{ T "comments_reply_count" (dict "Count" "__N__") }},
    loadOlderComments: {{ T "comments_load_older_comments" }},
    loadOlderReplies: {{ T "comments_load_older_replies" }},
    loadOlderRepliesRemaining: {{ T "comments_load_older_replies_remaining" (dict "Count" "__N__") }},
    loadRepliesTotal: {{ T "comments_load_replies_total" (dict "Count" "__N__") }},
    loadNewerReplies: {{ T "comments_load_newer_replies" }}
  };

  function applyHxEchoI18n(root) {
    var scope = root || document.getElementById('comments-section');
    if (!scope) return;
    var nodes = scope.querySelectorAll('[data-i18n]');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      var key = el.getAttribute('data-i18n');
      if (key === 'reply') {
        el.textContent = HX_ECHO_I18N.reply;
      } else if (key === 'reply-count') {
        var c = parseInt(el.getAttribute('data-count'), 10) || 0;
        el.textContent = c > 0
          ? (c === 1 ? HX_ECHO_I18N.replyCountOne : HX_ECHO_I18N.replyCountOther.replace('__N__', String(c)))
          : '';
      } else if (key === 'load-older-comments') {
        el.textContent = HX_ECHO_I18N.loadOlderComments;
      } else if (key === 'load-older-replies') {
        el.textContent = HX_ECHO_I18N.loadOlderReplies;
      } else if (key === 'load-older-replies-remaining') {
        el.textContent = HX_ECHO_I18N.loadOlderRepliesRemaining.replace('__N__', el.getAttribute('data-remaining') || '0');
      } else if (key === 'load-replies-total') {
        el.textContent = HX_ECHO_I18N.loadRepliesTotal.replace('__N__', el.getAttribute('data-total') || '0');
      } else if (key === 'load-newer-replies') {
        el.textContent = HX_ECHO_I18N.loadNewerReplies;
      }
    }
  }

  function formatHxEchoTimestamps(root) {
    var scope = root || document.getElementById('comments-section');
    if (!scope) return;
    var nodes = scope.querySelectorAll('time[data-ts]');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      if (el.dataset.formatted === '1') continue;
      var ts = parseInt(el.getAttribute('data-ts'), 10);
      if (!isNaN(ts) && ts > 0) {
        var d = new Date(ts * 1000);
        var pad = function (n) { return n < 10 ? '0' + n : '' + n; };
        el.textContent = d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) +
          ' ' + pad(d.getHours()) + ':' + pad(d.getMinutes());
        el.setAttribute('datetime', d.toISOString());
        el.dataset.formatted = '1';
      }
    }
  }

  function setReplyTarget(rootId, authorName) {
    var parentInput = document.getElementById('parent_id');
    var replyAuthorInput = document.getElementById('reply_to_author');
    var targetName = document.getElementById('reply-target-name');
    var indicator = document.getElementById('reply-indicator');
    var textarea = document.getElementById('comment-textarea');
    if (!parentInput || !replyAuthorInput || !targetName || !indicator || !textarea) return;

    parentInput.value = rootId;
    replyAuthorInput.value = authorName;
    targetName.textContent = '@' + authorName;
    indicator.style.display = 'flex';
    textarea.focus();
  }

  function clearReplyTarget() {
    var parentInput = document.getElementById('parent_id');
    var replyAuthorInput = document.getElementById('reply_to_author');
    var indicator = document.getElementById('reply-indicator');
    if (parentInput) parentInput.value = '';
    if (replyAuthorInput) replyAuthorInput.value = '';
    if (indicator) indicator.style.display = 'none';
  }

  function getHxEchoMaxCommentId() {
    var maxId = 0;
    var items = document.querySelectorAll('#comments-section li[id^="comment-"]');
    for (var i = 0; i < items.length; i++) {
      var match = /^comment-(\d+)$/.exec(items[i].id);
      if (match) {
        var id = parseInt(match[1], 10);
        if (id > maxId) maxId = id;
      }
    }
    return maxId;
  }

  function deduplicateHxEchoHtml(rawHtml) {
    if (!rawHtml) return rawHtml;
    var tpl = document.createElement('template');
    tpl.innerHTML = rawHtml;
    var nodes = tpl.content.querySelectorAll('li[id^="comment-"]');
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      if (/^comment-\d+$/.test(el.id) && document.getElementById(el.id)) {
        if (el.parentNode) el.parentNode.removeChild(el);
      }
    }
    return tpl.innerHTML;
  }

  (function () {
    formatHxEchoTimestamps();
    applyHxEchoI18n();
    var formEl = document.getElementById('comment-form');
    if (formEl && !formEl.__hxFallbackBound) {
      formEl.__hxFallbackBound = true;
      formEl.addEventListener('submit', function (e) {
        if (!window.htmx) e.preventDefault();
      });
    }
    if (window.__hxEchoEventsBound) return;
    window.__hxEchoEventsBound = true;

    document.addEventListener('htmx:configRequest', function (evt) {
      var elt = evt.detail && evt.detail.elt;
      var path = evt.detail && evt.detail.path;
      if (elt && elt.id === 'comment-list' && typeof path === 'string') {
        var maxId = getHxEchoMaxCommentId();
        if (maxId > 0) {
          evt.detail.path = path.replace(/([?&]after=)(\d+)/, function (_, prefix, cur) {
            var curNum = parseInt(cur, 10) || 0;
            return prefix + Math.max(curNum, maxId);
          });
        }
      }
    });

    document.addEventListener('htmx:beforeSwap', function (evt) {
      var target = evt.detail && evt.detail.target;
      if (!target) return;
      var section = document.getElementById('comments-section');
      if (!section || !section.contains(target)) return;
      if (typeof evt.detail.serverResponse === 'string' && evt.detail.serverResponse.indexOf('comment-') !== -1) {
        var filtered = deduplicateHxEchoHtml(evt.detail.serverResponse);
        evt.detail.serverResponse = filtered;
        if (target.id === 'comment-list' && !filtered.trim()) evt.detail.shouldSwap = false;
      }
    });

    document.addEventListener('htmx:beforeRequest', function (evt) {
      var form = document.getElementById('comment-form');
      if (form && evt.detail && evt.detail.elt === form) {
        var errEl = document.getElementById('comment-error');
        if (errEl) { errEl.textContent = ''; errEl.style.display = 'none'; }
      }
    });

    document.addEventListener('htmx:afterRequest', function (evt) {
      var form = document.getElementById('comment-form');
      if (form && evt.detail && evt.detail.elt === form) {
        if (evt.detail.successful) {
          form.reset();
          clearReplyTarget();
        } else {
          var errEl = document.getElementById('comment-error');
          if (errEl) {
            var xhr = evt.detail.xhr;
            errEl.textContent = (xhr && xhr.responseText) ? xhr.responseText.trim() : HX_ECHO_I18N.errorGeneric;
            errEl.style.display = 'block';
          }
        }
      }
    });

    document.addEventListener('htmx:afterSettle', function () {
      formatHxEchoTimestamps();
      applyHxEchoI18n();
    });
  })();
</script>
{{- end -}}
```

---

## 7. Implementation Status & Future Extensions

1. **Core Worker & Frontend Integration (Implemented)**:
   - `/api/comments/export` with item limits and `EXPORT_MAX_BYTES` byte circuit breaker.
   - `/api/comments` `GET` (delta `after`, root pagination `before`, thread reply pagination `parent_id` + `before`/`after`) and `POST` (honeypot, Markdown + `ammonia`, two-level tree normalization, `HX-Retarget` / `HX-Reswap`, OOB reply count).
   - Server-side i18n (`en`, `zh-cn`, `zh-tw`, `ja`) + semantic `data-i18n` attributes, `Cache-Control: no-store`, and loopback/configured CORS support.
   - 10 unit tests covering slug normalization, XSS/JS escaping, export packing, byte circuit breaker, delta/OOB rendering, pagination, CORS loopback validation, and locale parsing.
2. **Admin & Moderation Extension (Future)**:
   - Add `ADMIN_TOKEN` secret in Worker env for comment deletion/hiding (`status = 0`).
   - Optional debounced GitHub `repository_dispatch` webhook via Cloudflare KV/Queues to trigger static rebuilds after new comments settle.
3. **One-Click Self-Hosting (Future)**:
   - Provide a GitHub Actions workflow (`deploy.yml`) running `npx wrangler deploy` so forks can deploy without installing Rust locally.
