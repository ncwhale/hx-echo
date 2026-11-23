use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use ammonia::clean;
use pulldown_cmark::{html::push_html, Options, Parser};
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::JsValue;
use worker::*;

const MAX_AUTHOR_CHARS: usize = 50;
const MAX_CONTENT_CHARS: usize = 2500;

const DEFAULT_EXPORT_LIMIT: usize = 20;
const DEFAULT_EXPORT_MAX_LIMIT: usize = 100;
const DEFAULT_EXPORT_MAX_BYTES: usize = 32 * 1024;
const INITIAL_INLINE_REPLIES: usize = 5;
const HISTORY_REPLIES_PREVIEW_LIMIT: usize = 5;
const DEFAULT_PAGE_LIMIT: usize = 20;
const MAX_PAGE_LIMIT: usize = 100;

#[derive(Debug, Clone, Deserialize)]
struct CommentRow {
    id: i64,
    #[allow(dead_code)]
    slug: String,
    parent_id: Option<i64>,
    reply_to_author: Option<String>,
    author: String,
    content_html: String,
    created_at: i64,
    #[allow(dead_code)]
    status: i64,
    #[serde(default)]
    reply_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportResponse {
    last_cursor: i64,
    first_cursor: i64,
    has_more: bool,
    exported_count: usize,
    html_bytes: usize,
    html: String,
}

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let origin = req.headers().get("Origin").ok().flatten();
    let req_headers = req
        .headers()
        .get("Access-Control-Request-Headers")
        .ok()
        .flatten();
    let allowed_origin = origin
        .as_deref()
        .filter(|o| is_allowed_cors_origin(o, &env))
        .map(str::to_string);

    if req.method() == Method::Options {
        return handle_preflight(allowed_origin.as_deref(), req_headers.as_deref());
    }

    let resp = Router::new()
        .get_async("/api/comments/export", handle_export)
        .get_async("/api/comments", handle_get_comments)
        .post_async("/api/comments", handle_post_comment)
        .run(req, env)
        .await?;

    with_cors_headers(resp, allowed_origin.as_deref())
}

/// `GET /api/comments/export?slug=/posts/xxx/&limit=20&max_bytes=32768`
/// Exports the latest comments within the item limit and forced byte circuit breaker.
async fn handle_export(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let url = req.url()?;
    let Some(slug) = query_param(&url, "slug").and_then(|s| normalize_slug(&s)) else {
        return Response::error("Missing or invalid 'slug' query parameter", 400);
    };

    let admin_default_limit = env_usize(&ctx.env, "EXPORT_DEFAULT_LIMIT", DEFAULT_EXPORT_LIMIT);
    let admin_max_limit = env_usize(
        &ctx.env,
        "EXPORT_MAX_LIMIT",
        DEFAULT_EXPORT_MAX_LIMIT.max(admin_default_limit),
    )
    .max(admin_default_limit);
    let admin_max_bytes = env_usize(&ctx.env, "EXPORT_MAX_BYTES", DEFAULT_EXPORT_MAX_BYTES);

    let limit = match parse_optional_positive_usize(&url, &["limit"]) {
        Ok(Some(val)) => val.min(admin_max_limit),
        Ok(None) => admin_default_limit,
        Err(msg) => return Response::error(msg, 400),
    };

    // Forced byte circuit breaker: callers can tighten max_bytes, never exceed admin_max_bytes.
    let max_bytes = match parse_optional_positive_usize(&url, &["max_bytes", "maxBytes"]) {
        Ok(Some(val)) => val.min(admin_max_bytes),
        Ok(None) => admin_max_bytes,
        Err(msg) => return Response::error(msg, 400),
    };

    let db = ctx.env.d1("DB")?;
    let (locale, query_lang) = resolve_request_locale(&req, &url, None, &ctx.env);
    let render_ctx = RenderCtx {
        slug: &slug,
        locale,
        query_lang,
    };

    let roots_desc = fetch_roots_desc(&db, &slug, None, limit + 1).await?;

    let mut replies_desc_by_root: BTreeMap<i64, Vec<CommentRow>> = BTreeMap::new();
    let candidate_count = roots_desc.len().min(limit);
    if candidate_count > 0 {
        let min_root_id = roots_desc[candidate_count - 1].id;
        let replies_desc = fetch_replies_for_roots_desc(&db, &slug, min_root_id, limit).await?;
        for reply in replies_desc {
            if let Some(pid) = reply.parent_id {
                replies_desc_by_root.entry(pid).or_default().push(reply);
            }
        }
    }

    let export = build_export_package(
        &render_ctx,
        &roots_desc,
        &replies_desc_by_root,
        limit,
        max_bytes,
    );
    Response::from_json(&export)
}

/// `GET /api/comments`
/// Supports:
/// - `?slug=...&after=101` (runtime delta of newer comments; `204` if empty)
/// - `?slug=...&before=85&limit=20` (user-triggered HTMX loading of older top-level threads)
/// - `?slug=...&parent_id=101&before=112` / `&after=112` (user-triggered HTMX reply pagination)
async fn handle_get_comments(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let url = req.url()?;
    let Some(slug) = query_param(&url, "slug").and_then(|s| normalize_slug(&s)) else {
        return Response::error("Missing or invalid 'slug' query parameter", 400);
    };

    let parent_id = match parse_optional_positive_i64(&url, "parent_id") {
        Ok(v) => v,
        Err(msg) => return Response::error(msg, 400),
    };
    let before = match parse_optional_positive_i64(&url, "before") {
        Ok(v) => v,
        Err(msg) => return Response::error(msg, 400),
    };
    let after = match parse_optional_non_negative_i64(&url, "after") {
        Ok(v) => v,
        Err(msg) => return Response::error(msg, 400),
    };

    if before.is_some() && after.is_some() {
        return Response::error("Cannot specify both 'before' and 'after'", 400);
    }

    let limit = match parse_optional_positive_usize(&url, &["limit"]) {
        Ok(Some(val)) => val.min(MAX_PAGE_LIMIT),
        Ok(None) => DEFAULT_PAGE_LIMIT,
        Err(msg) => return Response::error(msg, 400),
    };

    let is_htmx = req
        .headers()
        .get("HX-Request")?
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let (locale, query_lang) = resolve_request_locale(&req, &url, None, &ctx.env);
    let render_ctx = RenderCtx {
        slug: &slug,
        locale,
        query_lang,
    };

    let db = ctx.env.d1("DB")?;

    // Mode 1: Thread-level reply pagination (`parent_id` provided).
    if let Some(pid) = parent_id {
        let html = if let Some(before_id) = before {
            let mut rows_desc = db
                .prepare(
                    "SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status \
                     FROM comments \
                     WHERE slug = ?1 AND status = 1 AND parent_id = ?2 AND id < ?3 \
                     ORDER BY id DESC LIMIT ?4",
                )
                .bind(&[
                    JsValue::from_str(&slug),
                    JsValue::from_f64(pid as f64),
                    JsValue::from_f64(before_id as f64),
                    JsValue::from_f64((limit + 1) as f64),
                ])?
                .all()
                .await?
                .results::<CommentRow>()?;

            let has_more_older = rows_desc.len() > limit;
            rows_desc.truncate(limit);
            rows_desc.reverse();
            build_thread_replies_html(&render_ctx, pid, &rows_desc, has_more_older, false)
        } else if let Some(after_id) = after {
            let mut rows_asc = db
                .prepare(
                    "SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status \
                     FROM comments \
                     WHERE slug = ?1 AND status = 1 AND parent_id = ?2 AND id > ?3 \
                     ORDER BY id ASC LIMIT ?4",
                )
                .bind(&[
                    JsValue::from_str(&slug),
                    JsValue::from_f64(pid as f64),
                    JsValue::from_f64(after_id as f64),
                    JsValue::from_f64((limit + 1) as f64),
                ])?
                .all()
                .await?
                .results::<CommentRow>()?;

            let has_more_newer = rows_asc.len() > limit;
            rows_asc.truncate(limit);
            build_thread_replies_html(&render_ctx, pid, &rows_asc, false, has_more_newer)
        } else {
            let mut rows_asc = db
                .prepare(
                    "SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status \
                     FROM comments \
                     WHERE slug = ?1 AND status = 1 AND parent_id = ?2 \
                     ORDER BY id ASC LIMIT ?3",
                )
                .bind(&[
                    JsValue::from_str(&slug),
                    JsValue::from_f64(pid as f64),
                    JsValue::from_f64((limit + 1) as f64),
                ])?
                .all()
                .await?
                .results::<CommentRow>()?;

            let has_more_newer = rows_asc.len() > limit;
            rows_asc.truncate(limit);
            build_thread_replies_html(&render_ctx, pid, &rows_asc, false, has_more_newer)
        };

        // Return 200 empty HTML so hx-swap="outerHTML" cleanly removes the trigger item.
        return Response::from_html(html);
    }

    // Mode 2: Older top-level comments pagination (`before` provided).
    if let Some(before_id) = before {
        let mut roots_desc = fetch_roots_desc(&db, &slug, Some(before_id), limit + 1).await?;
        let has_more_older = roots_desc.len() > limit;
        roots_desc.truncate(limit);

        if roots_desc.is_empty() {
            return Response::from_html("");
        }

        let min_root_id = roots_desc.last().map(|r| r.id).unwrap_or(0);
        let replies_desc =
            fetch_replies_for_roots_desc(&db, &slug, min_root_id, HISTORY_REPLIES_PREVIEW_LIMIT)
                .await?;
        let mut replies_by_root: BTreeMap<i64, Vec<CommentRow>> = BTreeMap::new();
        for reply in replies_desc {
            if let Some(pid) = reply.parent_id {
                replies_by_root.entry(pid).or_default().push(reply);
            }
        }

        roots_desc.reverse();
        let html = build_older_roots_html(&render_ctx, &roots_desc, &replies_by_root, has_more_older);
        return Response::from_html(html);
    }

    // Mode 3: Runtime delta loading (`after` provided or 0).
    let after_id = after.unwrap_or(0);
    let rows: Vec<CommentRow> = db
        .prepare(
            "SELECT c.id, c.slug, c.parent_id, c.reply_to_author, c.author, c.content_html, c.created_at, c.status, \
                    CASE WHEN c.parent_id IS NULL THEN \
                      (SELECT COUNT(*) FROM comments r WHERE r.parent_id = c.id AND r.status = 1) \
                    ELSE 0 END AS reply_count \
             FROM comments c \
             WHERE c.slug = ?1 AND c.status = 1 AND c.id > ?2 \
             ORDER BY c.id ASC",
        )
        .bind(&[JsValue::from_str(&slug), JsValue::from_f64(after_id as f64)])?
        .all()
        .await?
        .results()?;

    if rows.is_empty() {
        return empty_no_content();
    }

    let mut html = build_delta_html(&render_ctx, &rows);
    if html.is_empty() {
        return empty_no_content();
    }

    // Keep reply-count badges on already-rendered static roots up to date for HTMX clients.
    if is_htmx {
        let new_root_ids: HashSet<i64> = rows
            .iter()
            .filter(|r| r.parent_id.is_none())
            .map(|r| r.id)
            .collect();
        let mut oob_root_ids: Vec<i64> = Vec::new();
        for row in &rows {
            if let Some(pid) = row.parent_id {
                if !new_root_ids.contains(&pid) && !oob_root_ids.contains(&pid) {
                    oob_root_ids.push(pid);
                }
            }
        }
        for root_id in oob_root_ids {
            let cnt = fetch_reply_count(&db, root_id).await?;
            html.push_str(&render_reply_count_oob(root_id, cnt, locale));
        }
    }

    Response::from_html(html)
}

/// `POST /api/comments`
/// Validates honeypot, sanitizes Markdown, persists to D1, and returns the rendered `<li>`.
async fn handle_post_comment(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let is_htmx = req
        .headers()
        .get("HX-Request")?
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let form = match req.form_data().await {
        Ok(f) => f,
        Err(_) => return Response::error("Invalid form payload", 400),
    };

    // Honeypot anti-spam: non-empty hidden `website` field is silently ignored.
    if let Some(website) = form_field(&form, "website") {
        if !website.trim().is_empty() {
            return empty_no_content();
        }
    }

    let Some(slug) = form_field(&form, "slug").and_then(|s| normalize_slug(&s)) else {
        return Response::error("Missing or invalid 'slug'", 400);
    };

    let author = form_field(&form, "author")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if author.is_empty() || author.chars().count() > MAX_AUTHOR_CHARS {
        return Response::error("Author must be between 1 and 50 characters", 400);
    }

    let raw_content = form_field(&form, "content")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if raw_content.is_empty() || raw_content.chars().count() > MAX_CONTENT_CHARS {
        return Response::error("Content must be between 1 and 2500 characters", 400);
    }

    let content_html = markdown_to_safe_html(&raw_content);
    if content_html.trim().is_empty() {
        return Response::error("Comment content cannot be empty after sanitization", 400);
    }

    let requested_parent_id: Option<i64> = match form_field(&form, "parent_id") {
        Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<i64>() {
            Ok(id) if id > 0 => Some(id),
            _ => return Response::error("Invalid 'parent_id'", 400),
        },
        _ => None,
    };

    let db = ctx.env.d1("DB")?;

    // Enforce two-level tree normalization: parent_id always points to the root comment ID.
    let effective_parent_id = match requested_parent_id {
        Some(pid) => {
            #[derive(Deserialize)]
            struct ParentLookup {
                id: i64,
                parent_id: Option<i64>,
            }

            let parent_row: Option<ParentLookup> = db
                .prepare("SELECT id, parent_id FROM comments WHERE id = ?1 AND slug = ?2 AND status = 1")
                .bind(&[JsValue::from_f64(pid as f64), JsValue::from_str(&slug)])?
                .first(None)
                .await?;

            match parent_row {
                Some(row) => Some(row.parent_id.unwrap_or(row.id)),
                None => return Response::error("Parent comment not found", 400),
            }
        }
        None => None,
    };

    let reply_to_author = if effective_parent_id.is_some() {
        form_field(&form, "reply_to_author")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.chars().count() <= MAX_AUTHOR_CHARS)
    } else {
        None
    };

    let created_at = (Date::now().as_millis() / 1000) as i64;

    let parent_js = match effective_parent_id {
        Some(pid) => JsValue::from_f64(pid as f64),
        None => JsValue::NULL,
    };
    let reply_to_js = match &reply_to_author {
        Some(target) => JsValue::from_str(target),
        None => JsValue::NULL,
    };

    let inserted: Option<CommentRow> = db
        .prepare(
            "INSERT INTO comments (slug, parent_id, reply_to_author, author, content_html, created_at, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1) \
             RETURNING id, slug, parent_id, reply_to_author, author, content_html, created_at, status, 0 AS reply_count",
        )
        .bind(&[
            JsValue::from_str(&slug),
            parent_js,
            reply_to_js,
            JsValue::from_str(&author),
            JsValue::from_str(&content_html),
            JsValue::from_f64(created_at as f64),
        ])?
        .first(None)
        .await?;

    let url = req.url()?;
    let form_lang = form_field(&form, "lang");
    let (locale, query_lang) = resolve_request_locale(&req, &url, form_lang.as_deref(), &ctx.env);
    let render_ctx = RenderCtx {
        slug: &slug,
        locale,
        query_lang,
    };

    let comment = inserted.ok_or_else(|| Error::RustError("Failed to insert comment".into()))?;

    let mut html = match comment.parent_id {
        Some(root_id) => render_reply_item(&comment, root_id, false, locale),
        None => render_comment_node(&render_ctx, &comment, &[], 0, 0),
    };

    if is_htmx {
        if let Some(root_id) = comment.parent_id {
            let count = fetch_reply_count(&db, root_id).await?;
            html.push_str(&render_reply_count_oob(root_id, count, locale));
        }
    }

    let mut response = Response::from_html(html)?;
    if let Some(root_id) = comment.parent_id {
        let headers = response.headers_mut();
        headers.set("HX-Retarget", &format!("#replies-for-{}", root_id))?;
        headers.set("HX-Reswap", "beforeend")?;
    }
    Ok(response)
}

async fn fetch_roots_desc(
    db: &D1Database,
    slug: &str,
    before_id: Option<i64>,
    limit: usize,
) -> Result<Vec<CommentRow>> {
    match before_id {
        Some(before) => {
            db.prepare(
                "SELECT c.id, c.slug, c.parent_id, c.reply_to_author, c.author, c.content_html, c.created_at, c.status, \
                        (SELECT COUNT(*) FROM comments r WHERE r.parent_id = c.id AND r.status = 1) AS reply_count \
                 FROM comments c \
                 WHERE c.slug = ?1 AND c.status = 1 AND c.parent_id IS NULL AND c.id < ?2 \
                 ORDER BY c.id DESC LIMIT ?3",
            )
            .bind(&[
                JsValue::from_str(slug),
                JsValue::from_f64(before as f64),
                JsValue::from_f64(limit as f64),
            ])?
            .all()
            .await?
            .results()
        }
        None => {
            db.prepare(
                "SELECT c.id, c.slug, c.parent_id, c.reply_to_author, c.author, c.content_html, c.created_at, c.status, \
                        (SELECT COUNT(*) FROM comments r WHERE r.parent_id = c.id AND r.status = 1) AS reply_count \
                 FROM comments c \
                 WHERE c.slug = ?1 AND c.status = 1 AND c.parent_id IS NULL \
                 ORDER BY c.id DESC LIMIT ?2",
            )
            .bind(&[JsValue::from_str(slug), JsValue::from_f64(limit as f64)])?
            .all()
            .await?
            .results()
        }
    }
}

async fn fetch_replies_for_roots_desc(
    db: &D1Database,
    slug: &str,
    min_root_id: i64,
    per_root_limit: usize,
) -> Result<Vec<CommentRow>> {
    db.prepare(
        "SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status, 0 AS reply_count \
         FROM ( \
           SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status, \
                  ROW_NUMBER() OVER (PARTITION BY parent_id ORDER BY id DESC) AS rn \
           FROM comments \
           WHERE slug = ?1 AND status = 1 AND parent_id >= ?2 \
         ) \
         WHERE rn <= ?3 \
         ORDER BY id DESC",
    )
    .bind(&[
        JsValue::from_str(slug),
        JsValue::from_f64(min_root_id as f64),
        JsValue::from_f64(per_root_limit as f64),
    ])?
    .all()
    .await?
    .results()
}

async fn fetch_reply_count(db: &D1Database, root_id: i64) -> Result<i64> {
    #[derive(Deserialize)]
    struct CountRow {
        cnt: i64,
    }
    let row: Option<CountRow> = db
        .prepare("SELECT COUNT(*) AS cnt FROM comments WHERE parent_id = ?1 AND status = 1")
        .bind(&[JsValue::from_f64(root_id as f64)])?
        .first(None)
        .await?;
    Ok(row.map(|r| r.cnt).unwrap_or(0))
}

fn env_usize(env: &Env, name: &str, default: usize) -> usize {
    if let Ok(var) = env.var(name) {
        if let Ok(val) = var.to_string().trim().parse::<usize>() {
            if val > 0 {
                return val;
            }
        }
    }
    if let Ok(val) = env.object_var::<usize>(name) {
        if val > 0 {
            return val;
        }
    }
    default
}

fn empty_no_content() -> Result<Response> {
    Ok(Response::empty()?.with_status(204))
}

fn is_allowed_loopback_origin(origin: &str) -> bool {
    let Ok(url) = Url::parse(origin) else {
        return false;
    };
    if url.scheme() != "http" || url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return false;
    }
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

fn is_allowed_cors_origin(origin: &str, env: &Env) -> bool {
    if is_allowed_loopback_origin(origin) {
        return true;
    }
    if let Ok(var) = env.var("CORS_ALLOW_ORIGINS") {
        let raw = var.to_string();
        for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if entry == origin {
                return true;
            }
        }
    }
    false
}

fn handle_preflight(
    allowed_origin: Option<&str>,
    requested_headers: Option<&str>,
) -> Result<Response> {
    let mut resp = empty_no_content()?;
    if let Some(origin) = allowed_origin {
        let allow_headers = requested_headers
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Content-Type, HX-Request, HX-Target, HX-Trigger, HX-Trigger-Name, HX-Current-URL, HX-Prompt");
        let headers = resp.headers_mut();
        headers.set("Access-Control-Allow-Origin", origin)?;
        headers.set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
        headers.set("Access-Control-Allow-Headers", allow_headers)?;
        headers.set(
            "Access-Control-Expose-Headers",
            "HX-Retarget, HX-Reswap, HX-Reselect, HX-Trigger, HX-Trigger-After-Swap, HX-Trigger-After-Settle, HX-Refresh, HX-Redirect, HX-Location, HX-Push, HX-Push-Url, HX-Replace-Url",
        )?;
        headers.set("Access-Control-Max-Age", "86400")?;
        headers.set("Vary", "Origin")?;
    }
    Ok(resp)
}

fn with_cors_headers(mut resp: Response, allowed_origin: Option<&str>) -> Result<Response> {
    let headers = resp.headers_mut();
    headers.set("Cache-Control", "no-store")?;
    if let Some(origin) = allowed_origin {
        headers.set("Access-Control-Allow-Origin", origin)?;
        headers.set(
            "Access-Control-Expose-Headers",
            "HX-Retarget, HX-Reswap, HX-Reselect, HX-Trigger, HX-Trigger-After-Swap, HX-Trigger-After-Settle, HX-Refresh, HX-Redirect, HX-Location, HX-Push, HX-Push-Url, HX-Replace-Url",
        )?;
        headers.set("Vary", "Origin")?;
    }
    Ok(resp)
}

fn query_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn parse_optional_positive_usize(
    url: &Url,
    keys: &[&str],
) -> std::result::Result<Option<usize>, &'static str> {
    for key in keys {
        if let Some(raw) = query_param(url, key) {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            return match trimmed.parse::<usize>() {
                Ok(v) if v > 0 => Ok(Some(v)),
                _ => Err("Query parameter must be a positive integer"),
            };
        }
    }
    Ok(None)
}

fn parse_optional_positive_i64(
    url: &Url,
    key: &str,
) -> std::result::Result<Option<i64>, &'static str> {
    let Some(raw) = query_param(url, key) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<i64>() {
        Ok(v) if v > 0 => Ok(Some(v)),
        _ => Err("Invalid query cursor parameter"),
    }
}

fn parse_optional_non_negative_i64(
    url: &Url,
    key: &str,
) -> std::result::Result<Option<i64>, &'static str> {
    let Some(raw) = query_param(url, key) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<i64>() {
        Ok(v) if v >= 0 => Ok(Some(v)),
        _ => Err("Invalid 'after' cursor"),
    }
}

fn form_field(form: &FormData, key: &str) -> Option<String> {
    match form.get(key) {
        Some(FormEntry::Field(val)) => Some(val),
        _ => None,
    }
}

/// Normalizes an article identifier to a pathname-only `RelPermalink` (e.g. `/posts/hello-world/`).
fn normalize_slug(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Url::parse(trimmed).ok()?.path().to_string()
    } else {
        let without_fragment = trimmed.split('#').next().unwrap_or("");
        let without_query = without_fragment.split('?').next().unwrap_or("");
        if without_query.is_empty() {
            return None;
        }
        if without_query.starts_with('/') {
            without_query.to_string()
        } else {
            format!("/{}", without_query)
        }
    };

    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Parses Markdown into HTML and sanitizes it via `ammonia` to prevent XSS.
fn markdown_to_safe_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(markdown, options);
    let mut unsafe_html = String::new();
    push_html(&mut unsafe_html, parser);
    clean(&unsafe_html)
}

/// Escapes special HTML characters for safe interpolation in text nodes and attributes.
fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escapes a string for safe embedding inside a single-quoted JS literal inside an HTML attribute.
fn escape_js_single_quoted(input: &str) -> String {
    let mut js_escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => js_escaped.push_str("\\\\"),
            '\'' => js_escaped.push_str("\\'"),
            '"' => js_escaped.push_str("\\\""),
            '\n' => js_escaped.push_str("\\n"),
            '\r' => js_escaped.push_str("\\r"),
            '\t' => js_escaped.push_str("\\t"),
            '<' => js_escaped.push_str("\\u003c"),
            '>' => js_escaped.push_str("\\u003e"),
            '&' => js_escaped.push_str("\\u0026"),
            '\u{2028}' => js_escaped.push_str("\\u2028"),
            '\u{2029}' => js_escaped.push_str("\\u2029"),
            _ => js_escaped.push(ch),
        }
    }
    escape_html(&js_escaped)
}

/// Percent-encodes a UTF-8 string for safe inclusion as a URL query parameter value.
fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => {
                let _ = write!(out, "%{:02X}", byte);
            }
        }
    }
    out
}

struct ExportEntry<'a> {
    root: &'a CommentRow,
    replies_desc: Vec<&'a CommentRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locale {
    En,
    ZhCn,
    ZhTw,
    Ja,
}

struct RenderCtx<'a> {
    slug: &'a str,
    locale: Locale,
    query_lang: Option<Locale>,
}

impl Locale {
    fn code(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhCn => "zh-cn",
            Locale::ZhTw => "zh-tw",
            Locale::Ja => "ja",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase().replace('_', "-");
        if norm.is_empty() {
            return None;
        }
        if norm == "en" || norm.starts_with("en-") {
            return Some(Locale::En);
        }
        if norm == "ja" || norm.starts_with("ja-") {
            return Some(Locale::Ja);
        }
        if matches!(norm.as_str(), "zh-tw" | "zh-hk" | "zh-mo" | "zh-hant")
            || norm.starts_with("zh-hant-")
        {
            return Some(Locale::ZhTw);
        }
        if norm == "zh" || norm == "zh-cn" || norm == "zh-sg" || norm.starts_with("zh-hans") || norm.starts_with("zh-cn-") {
            return Some(Locale::ZhCn);
        }
        None
    }

    fn from_accept_language(header: &str) -> Option<Self> {
        let mut best: Option<(f32, usize, Locale)> = None;
        for (idx, part) in header.split(',').enumerate() {
            let mut segments = part.split(';');
            let tag = segments.next().unwrap_or("").trim();
            if tag.is_empty() || tag == "*" {
                continue;
            }
            let mut q = 1.0_f32;
            for param in segments {
                let p = param.trim();
                if let Some(val) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                    if let Ok(parsed) = val.trim().parse::<f32>() {
                        q = parsed;
                    }
                }
            }
            if q <= 0.0 {
                continue;
            }
            if let Some(loc) = Locale::parse(tag) {
                match best {
                    None => best = Some((q, idx, loc)),
                    Some((best_q, best_idx, _)) if q > best_q || (q == best_q && idx < best_idx) => {
                        best = Some((q, idx, loc));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(_, _, loc)| loc)
    }

    fn reply_label(self) -> &'static str {
        match self {
            Locale::En => "Reply",
            Locale::ZhCn => "回复",
            Locale::ZhTw => "回覆",
            Locale::Ja => "返信",
        }
    }

    fn format_reply_count(self, count: i64) -> String {
        match self {
            Locale::En => {
                if count == 1 {
                    "1 reply".to_string()
                } else {
                    format!("{count} replies")
                }
            }
            Locale::ZhCn => format!("{count} 条回复"),
            Locale::ZhTw => format!("{count} 則回覆"),
            Locale::Ja => format!("{count} 件の返信"),
        }
    }

    fn load_older_comments_label(self) -> &'static str {
        match self {
            Locale::En => "Load older comments",
            Locale::ZhCn => "加载更早评论",
            Locale::ZhTw => "載入較早評論",
            Locale::Ja => "以前のコメントを読み込む",
        }
    }

    fn load_older_replies_remaining_label(self, remaining: usize) -> String {
        match self {
            Locale::En => format!("Load older replies ({remaining} more)"),
            Locale::ZhCn => format!("加载更早回复 (还有 {remaining} 条)"),
            Locale::ZhTw => format!("載入較早回覆 (還有 {remaining} 則)"),
            Locale::Ja => format!("以前の返信を読み込む (残り {remaining} 件)"),
        }
    }

    fn load_replies_total_label(self, total: usize) -> String {
        match self {
            Locale::En => format!("Load replies ({total} total)"),
            Locale::ZhCn => format!("加载回复 (共 {total} 条)"),
            Locale::ZhTw => format!("載入回覆 (共 {total} 則)"),
            Locale::Ja => format!("返信を読み込む (全 {total} 件)"),
        }
    }

    fn load_older_replies_label(self) -> &'static str {
        match self {
            Locale::En => "Load older replies",
            Locale::ZhCn => "加载更早回复",
            Locale::ZhTw => "載入較早回覆",
            Locale::Ja => "以前の返信を読み込む",
        }
    }

    fn load_newer_replies_label(self) -> &'static str {
        match self {
            Locale::En => "Load more replies",
            Locale::ZhCn => "加载更多回复",
            Locale::ZhTw => "載入更多回覆",
            Locale::Ja => "さらに返信を読み込む",
        }
    }
}

fn resolve_request_locale(
    req: &Request,
    url: &Url,
    form_lang: Option<&str>,
    env: &Env,
) -> (Locale, Option<Locale>) {
    if let Some(loc) = form_lang.and_then(Locale::parse) {
        return (loc, Some(loc));
    }
    if let Some(loc) = query_param(url, "lang").and_then(|s| Locale::parse(&s)) {
        return (loc, Some(loc));
    }
    if let Ok(Some(hx_lang)) = req.headers().get("HX-Lang") {
        if let Some(loc) = Locale::parse(&hx_lang) {
            return (loc, None);
        }
    }
    if let Ok(Some(accept)) = req.headers().get("Accept-Language") {
        if let Some(loc) = Locale::from_accept_language(&accept) {
            return (loc, None);
        }
    }
    if let Ok(var) = env.var("DEFAULT_LANG") {
        if let Some(loc) = Locale::parse(&var.to_string()) {
            return (loc, None);
        }
    }
    (Locale::En, None)
}

/// Packs the newest comments within `limit` items and `max_bytes` UTF-8 bytes,
/// emitting HTMX pagination triggers where history or replies are truncated.
fn build_export_package(
    ctx: &RenderCtx<'_>,
    roots_desc: &[CommentRow],
    replies_desc_by_root: &BTreeMap<i64, Vec<CommentRow>>,
    limit: usize,
    max_bytes: usize,
) -> ExportResponse {
    let has_older_roots_in_db = roots_desc.len() > limit;
    let candidates = &roots_desc[..roots_desc.len().min(limit)];

    if candidates.is_empty() {
        return ExportResponse {
            last_cursor: 0,
            first_cursor: 0,
            has_more: false,
            exported_count: 0,
            html_bytes: 0,
            html: String::new(),
        };
    }

    let mut accepted: Vec<ExportEntry<'_>> = Vec::new();
    let mut used_items: usize = 0;

    // Pass 1: include newest roots and up to INITIAL_INLINE_REPLIES per root.
    for (idx, root) in candidates.iter().enumerate() {
        if used_items + 1 > limit {
            break;
        }
        let remaining_roots_exist = has_older_roots_in_db || (idx + 1 < candidates.len());
        accepted.push(ExportEntry {
            root,
            replies_desc: Vec::new(),
        });

        let candidate_html = render_export_entries(ctx, &accepted, remaining_roots_exist);
        if candidate_html.len() > max_bytes {
            accepted.pop();
            break;
        }
        used_items += 1;

        if let Some(root_replies) = replies_desc_by_root.get(&root.id) {
            for reply in root_replies.iter().take(INITIAL_INLINE_REPLIES) {
                if used_items + 1 > limit {
                    break;
                }
                accepted.last_mut().unwrap().replies_desc.push(reply);
                let candidate_html = render_export_entries(ctx, &accepted, remaining_roots_exist);
                if candidate_html.len() > max_bytes {
                    accepted.last_mut().unwrap().replies_desc.pop();
                    break;
                }
                used_items += 1;
            }
        }
    }

    let has_more_roots = has_older_roots_in_db || accepted.len() < candidates.len();

    // Pass 2: fill any remaining item/byte budget with additional replies for accepted roots.
    for idx in 0..accepted.len() {
        if used_items >= limit {
            break;
        }
        let root_id = accepted[idx].root.id;
        let already = accepted[idx].replies_desc.len();
        if let Some(root_replies) = replies_desc_by_root.get(&root_id) {
            for reply in root_replies.iter().skip(already) {
                if used_items + 1 > limit {
                    break;
                }
                accepted[idx].replies_desc.push(reply);
                let candidate_html = render_export_entries(ctx, &accepted, has_more_roots);
                if candidate_html.len() > max_bytes {
                    accepted[idx].replies_desc.pop();
                    break;
                }
                used_items += 1;
            }
        }
    }

    let html = render_export_entries(ctx, &accepted, has_more_roots);
    let html_bytes = html.len();
    let first_cursor = accepted.last().map(|e| e.root.id).unwrap_or(0);

    let mut last_cursor = 0_i64;
    for entry in &accepted {
        last_cursor = last_cursor.max(entry.root.id);
        for reply in &entry.replies_desc {
            last_cursor = last_cursor.max(reply.id);
        }
    }

    let has_truncated_replies = accepted.iter().any(|e| {
        let total = effective_reply_count(e.root, e.replies_desc.len());
        (total as usize) > e.replies_desc.len()
    });
    let has_more = has_more_roots || has_truncated_replies || (accepted.is_empty() && !candidates.is_empty());

    ExportResponse {
        last_cursor,
        first_cursor,
        has_more,
        exported_count: used_items,
        html_bytes,
        html,
    }
}

fn render_export_entries(
    ctx: &RenderCtx<'_>,
    entries_desc: &[ExportEntry<'_>],
    has_more_roots: bool,
) -> String {
    if entries_desc.is_empty() {
        return String::new();
    }

    let mut html = String::new();
    if has_more_roots {
        let oldest_root_id = entries_desc.last().unwrap().root.id;
        html.push_str(&render_load_older_comments_item(ctx, oldest_root_id));
    }

    for entry in entries_desc.iter().rev() {
        let mut children_asc: Vec<&CommentRow> = entry.replies_desc.clone();
        children_asc.reverse();
        let total_replies = effective_reply_count(entry.root, children_asc.len());
        let hidden_older = (total_replies as usize).saturating_sub(children_asc.len());
        html.push_str(&render_comment_node(
            ctx,
            entry.root,
            &children_asc,
            total_replies,
            hidden_older,
        ));
    }

    html
}

fn build_older_roots_html(
    ctx: &RenderCtx<'_>,
    roots_asc: &[CommentRow],
    replies_desc_by_root: &BTreeMap<i64, Vec<CommentRow>>,
    has_more_older: bool,
) -> String {
    if roots_asc.is_empty() {
        return String::new();
    }

    let mut html = String::new();
    if has_more_older {
        html.push_str(&render_load_older_comments_item(ctx, roots_asc[0].id));
    }

    for root in roots_asc {
        let mut children_asc: Vec<&CommentRow> = replies_desc_by_root
            .get(&root.id)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        children_asc.reverse();
        let total_replies = effective_reply_count(root, children_asc.len());
        let hidden_older = (total_replies as usize).saturating_sub(children_asc.len());
        html.push_str(&render_comment_node(
            ctx,
            root,
            &children_asc,
            total_replies,
            hidden_older,
        ));
    }

    html
}

fn build_thread_replies_html(
    ctx: &RenderCtx<'_>,
    root_id: i64,
    replies_asc: &[CommentRow],
    has_more_older: bool,
    has_more_newer: bool,
) -> String {
    if replies_asc.is_empty() {
        return String::new();
    }

    let mut html = String::new();
    if has_more_older {
        html.push_str(&render_load_older_replies_item(
            ctx,
            root_id,
            Some(replies_asc[0].id),
            None,
        ));
    }
    for reply in replies_asc {
        html.push_str(&render_reply_item(reply, root_id, false, ctx.locale));
    }
    if has_more_newer {
        let last_id = replies_asc.last().unwrap().id;
        html.push_str(&render_load_newer_replies_item(ctx, root_id, last_id));
    }
    html
}

/// Builds incremental HTMX fragments for comments newer than `after`.
/// Replies to new top-level comments in the same batch are nested directly;
/// replies to existing static comments carry `hx-swap-oob="beforeend:#replies-for-{parent_id}"`.
fn build_delta_html(ctx: &RenderCtx<'_>, rows: &[CommentRow]) -> String {
    let new_root_ids: HashSet<i64> = rows
        .iter()
        .filter(|r| r.parent_id.is_none())
        .map(|r| r.id)
        .collect();

    let mut roots: Vec<&CommentRow> = Vec::new();
    let mut nested_children: BTreeMap<i64, Vec<&CommentRow>> = BTreeMap::new();
    let mut oob_replies: Vec<&CommentRow> = Vec::new();

    for row in rows {
        match row.parent_id {
            None => roots.push(row),
            Some(pid) if new_root_ids.contains(&pid) => {
                nested_children.entry(pid).or_default().push(row);
            }
            Some(_) => oob_replies.push(row),
        }
    }

    let mut html = String::new();
    for root in roots {
        let children = nested_children
            .get(&root.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let total_replies = effective_reply_count(root, children.len());
        let hidden_older = (total_replies as usize).saturating_sub(children.len());
        html.push_str(&render_comment_node(
            ctx,
            root,
            children,
            total_replies,
            hidden_older,
        ));
    }
    for reply in oob_replies {
        if let Some(root_id) = reply.parent_id {
            html.push_str(&render_reply_item(reply, root_id, true, ctx.locale));
        }
    }

    html
}

fn effective_reply_count(root: &CommentRow, rendered_children_len: usize) -> i64 {
    root.reply_count.max(rendered_children_len as i64)
}

fn render_comment_node(
    ctx: &RenderCtx<'_>,
    comment: &CommentRow,
    children: &[&CommentRow],
    reply_count: i64,
    hidden_older_replies: usize,
) -> String {
    let author_html = escape_html(&comment.author);
    let author_js = escape_js_single_quoted(&comment.author);
    let reply_count_badge = render_reply_count_span(comment.id, reply_count, false, ctx.locale);
    let reply_label = ctx.locale.reply_label();

    let mut out = String::new();
    let _ = write!(
        out,
        r#"<li id="comment-{id}" class="comment-item" data-reply-count="{reply_count}"><div class="comment-meta"><strong>{author}</strong><time data-ts="{ts}">{ts}</time>{reply_count_badge}<button type="button" class="reply-btn" data-i18n="reply" onclick="setReplyTarget({id}, '{author_js}')">{reply_label}</button></div><div class="comment-body">{body}</div><ul id="replies-for-{id}" class="comment-replies">"#,
        id = comment.id,
        reply_count = reply_count,
        author = author_html,
        ts = comment.created_at,
        reply_count_badge = reply_count_badge,
        author_js = author_js,
        reply_label = reply_label,
        body = comment.content_html,
    );

    if hidden_older_replies > 0 {
        let before_id = children.first().map(|c| c.id);
        out.push_str(&render_load_older_replies_item(
            ctx,
            comment.id,
            before_id,
            Some(hidden_older_replies),
        ));
    }

    for child in children {
        out.push_str(&render_reply_item(child, comment.id, false, ctx.locale));
    }

    out.push_str("</ul></li>");
    out
}

fn render_reply_count_span(root_id: i64, count: i64, oob: bool, locale: Locale) -> String {
    let oob_attr = if oob {
        r#" hx-swap-oob="outerHTML""#
    } else {
        ""
    };
    if count > 0 {
        let text = locale.format_reply_count(count);
        format!(
            r#"<span id="reply-count-{root_id}" class="reply-count" data-count="{count}" data-i18n="reply-count"{oob_attr}>{text}</span>"#,
        )
    } else {
        format!(
            r#"<span id="reply-count-{root_id}" class="reply-count" data-count="0" data-i18n="reply-count"{oob_attr}></span>"#,
        )
    }
}

fn render_reply_count_oob(root_id: i64, count: i64, locale: Locale) -> String {
    render_reply_count_span(root_id, count, true, locale)
}

fn lang_query_suffix(query_lang: Option<Locale>) -> String {
    match query_lang {
        Some(loc) => format!("&amp;lang={}", loc.code()),
        None => String::new(),
    }
}

fn render_load_older_comments_item(ctx: &RenderCtx<'_>, before_id: i64) -> String {
    let encoded_slug = encode_uri_component(ctx.slug);
    let lang_suffix = lang_query_suffix(ctx.query_lang);
    let label = ctx.locale.load_older_comments_label();
    format!(
        r##"<li id="load-older-comments" class="load-older-item"><button type="button" class="load-older-btn" data-i18n="load-older-comments" hx-get="/api/comments?slug={encoded_slug}&amp;before={before_id}{lang_suffix}" hx-target="#load-older-comments" hx-swap="outerHTML">{label}</button></li>"##,
    )
}

fn render_load_older_replies_item(
    ctx: &RenderCtx<'_>,
    root_id: i64,
    before_id: Option<i64>,
    remaining_hint: Option<usize>,
) -> String {
    let encoded_slug = encode_uri_component(ctx.slug);
    let lang_suffix = lang_query_suffix(ctx.query_lang);
    let cursor_param = match before_id {
        Some(bid) => format!("&amp;before={}", bid),
        None => String::new(),
    };
    let (i18n_attrs, label) = match (before_id, remaining_hint) {
        (Some(_), Some(n)) => (
            format!(r#"data-i18n="load-older-replies-remaining" data-remaining="{n}""#),
            ctx.locale.load_older_replies_remaining_label(n),
        ),
        (None, Some(n)) => (
            format!(r#"data-i18n="load-replies-total" data-total="{n}""#),
            ctx.locale.load_replies_total_label(n),
        ),
        _ => (
            r#"data-i18n="load-older-replies""#.to_string(),
            ctx.locale.load_older_replies_label().to_string(),
        ),
    };
    format!(
        r##"<li id="load-older-replies-{root_id}" class="load-more-replies"><button type="button" class="load-replies-btn" {i18n_attrs} hx-get="/api/comments?slug={encoded_slug}&amp;parent_id={root_id}{cursor_param}{lang_suffix}" hx-target="#load-older-replies-{root_id}" hx-swap="outerHTML">{label}</button></li>"##,
    )
}

fn render_load_newer_replies_item(ctx: &RenderCtx<'_>, root_id: i64, after_id: i64) -> String {
    let encoded_slug = encode_uri_component(ctx.slug);
    let lang_suffix = lang_query_suffix(ctx.query_lang);
    let label = ctx.locale.load_newer_replies_label();
    format!(
        r##"<li id="load-newer-replies-{root_id}" class="load-more-replies"><button type="button" class="load-replies-btn" data-i18n="load-newer-replies" hx-get="/api/comments?slug={encoded_slug}&amp;parent_id={root_id}&amp;after={after_id}{lang_suffix}" hx-target="#load-newer-replies-{root_id}" hx-swap="outerHTML">{label}</button></li>"##,
    )
}

fn render_reply_item(reply: &CommentRow, root_id: i64, oob: bool, locale: Locale) -> String {
    let author_html = escape_html(&reply.author);
    let author_js = escape_js_single_quoted(&reply.author);
    let reply_label = locale.reply_label();

    let oob_attr = if oob {
        format!(r#" hx-swap-oob="beforeend:#replies-for-{}""#, root_id)
    } else {
        String::new()
    };

    let reply_to_prefix = match reply.reply_to_author.as_deref().map(str::trim) {
        Some(target) if !target.is_empty() => {
            format!(r#"<span class="reply-to">@{}</span> "#, escape_html(target))
        }
        _ => String::new(),
    };

    format!(
        r#"<li id="comment-{id}" class="comment-reply-item"{oob}><div class="comment-meta"><strong>{author}</strong><time data-ts="{ts}">{ts}</time><button type="button" class="reply-btn" data-i18n="reply" onclick="setReplyTarget({root_id}, '{author_js}')">{reply_label}</button></div><div class="comment-body">{prefix}{body}</div></li>"#,
        id = reply.id,
        oob = oob_attr,
        author = author_html,
        ts = reply.created_at,
        root_id = root_id,
        author_js = author_js,
        reply_label = reply_label,
        prefix = reply_to_prefix,
        body = reply.content_html,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(
        id: i64,
        parent_id: Option<i64>,
        reply_to_author: Option<&str>,
        author: &str,
        content_html: &str,
        created_at: i64,
    ) -> CommentRow {
        CommentRow {
            id,
            slug: "/posts/hello-world/".to_string(),
            parent_id,
            reply_to_author: reply_to_author.map(str::to_string),
            author: author.to_string(),
            content_html: content_html.to_string(),
            created_at,
            status: 1,
            reply_count: 0,
        }
    }

    #[test]
    fn test_normalize_slug() {
        assert_eq!(
            normalize_slug("/posts/hello-world/"),
            Some("/posts/hello-world/".to_string())
        );
        assert_eq!(
            normalize_slug("https://mirror.example.org/posts/hello-world/?ref=hn#comments"),
            Some("/posts/hello-world/".to_string())
        );
        assert_eq!(
            normalize_slug("posts/hello-world/?a=1"),
            Some("/posts/hello-world/".to_string())
        );
        assert_eq!(normalize_slug("   "), None);
    }

    #[test]
    fn test_escape_js_single_quoted_prevents_injection() {
        let payload = "Alice'); alert('xss'); //";
        let escaped = escape_js_single_quoted(payload);
        assert!(!escaped.contains('\''));
        assert!(escaped.contains("\\&#x27;"));
    }

    #[test]
    fn test_markdown_sanitization() {
        let md = "Hello **world**! <script>alert(1)</script> [bad](javascript:alert(1))";
        let html = markdown_to_safe_html(md);
        assert!(html.contains("<strong>world</strong>"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn test_export_package_respects_limit_and_reply_count() {
        let mut r1 = make_row(101, None, None, "Alice", "<p>Root 1</p>", 1732123456);
        r1.reply_count = 1;
        let mut r2 = make_row(103, None, None, "Carol", "<p>Root 2</p>", 1732123600);
        r2.reply_count = 2;

        let c1 = make_row(102, Some(101), Some("Alice"), "Bob", "<p>R1</p>", 1732123500);
        let c2 = make_row(104, Some(103), Some("Carol"), "Dave", "<p>R2a</p>", 1732123650);
        let c3 = make_row(105, Some(103), Some("Dave"), "Eve", "<p>R2b</p>", 1732123700);

        let roots_desc = vec![r2, r1];
        let mut replies_desc_by_root = BTreeMap::new();
        replies_desc_by_root.insert(101, vec![c1]);
        replies_desc_by_root.insert(103, vec![c3, c2]);

        let ctx = RenderCtx {
            slug: "/posts/hello-world/",
            locale: Locale::ZhCn,
            query_lang: Some(Locale::ZhCn),
        };

        // Limit = 3 items -> accepts r2 + c3 + c2 (3 items), truncates r1.
        let pkg = build_export_package(
            &ctx,
            &roots_desc,
            &replies_desc_by_root,
            3,
            DEFAULT_EXPORT_MAX_BYTES,
        );
        assert_eq!(pkg.exported_count, 3);
        assert_eq!(pkg.first_cursor, 103);
        assert_eq!(pkg.last_cursor, 105);
        assert!(pkg.has_more);
        assert!(pkg.html.contains(r#"id="load-older-comments""#));
        assert!(pkg.html.contains("before=103&amp;lang=zh-cn"));
        assert!(pkg.html.contains(r#"data-i18n="load-older-comments""#));
        assert!(pkg.html.contains(r#"data-reply-count="2""#));
        assert!(pkg.html.contains(">2 条回复</span>"));
        assert!(!pkg.html.contains(r#"id="comment-101""#));
    }

    #[test]
    fn test_export_package_byte_circuit_breaker() {
        let r1 = make_row(101, None, None, "Alice", &format!("<p>{}</p>", "a".repeat(600)), 1732123456);
        let r2 = make_row(102, None, None, "Bob", &format!("<p>{}</p>", "b".repeat(600)), 1732123500);
        let r3 = make_row(103, None, None, "Carol", &format!("<p>{}</p>", "c".repeat(600)), 1732123600);

        let roots_desc = vec![r3, r2, r1];
        let replies_desc_by_root = BTreeMap::new();
        let ctx = RenderCtx {
            slug: "/posts/hello-world/",
            locale: Locale::En,
            query_lang: None,
        };

        // Budget of 1400 bytes fits 1 root (~900 bytes + load-older button ~260 bytes) but not 2 roots.
        let pkg = build_export_package(
            &ctx,
            &roots_desc,
            &replies_desc_by_root,
            20,
            1400,
        );
        assert_eq!(pkg.exported_count, 1);
        assert!(pkg.html_bytes <= 1400);
        assert_eq!(pkg.html_bytes, pkg.html.len());
        assert!(pkg.has_more);
        assert!(pkg.html.contains(r#"id="comment-103""#));
        assert!(!pkg.html.contains(r#"id="comment-102""#));
        assert!(pkg.html.contains("before=103"));
        assert!(pkg.html.contains(">Load older comments</button>"));

        // Budget smaller than a single comment strictly yields 0 bytes.
        let tiny = build_export_package(
            &ctx,
            &roots_desc,
            &replies_desc_by_root,
            20,
            100,
        );
        assert_eq!(tiny.exported_count, 0);
        assert_eq!(tiny.html_bytes, 0);
        assert!(tiny.has_more);
    }

    #[test]
    fn test_export_package_truncates_replies_with_load_older_replies_trigger() {
        let mut r1 = make_row(101, None, None, "Alice", "<p>Root</p>", 1732123456);
        r1.reply_count = 4;

        let c1 = make_row(102, Some(101), Some("Alice"), "B1", "<p>1</p>", 1732123460);
        let c2 = make_row(103, Some(101), Some("Alice"), "B2", "<p>2</p>", 1732123470);
        let c3 = make_row(104, Some(101), Some("Alice"), "B3", "<p>3</p>", 1732123480);
        let c4 = make_row(105, Some(101), Some("Alice"), "B4", "<p>4</p>", 1732123490);

        let roots_desc = vec![r1];
        let mut replies_desc_by_root = BTreeMap::new();
        replies_desc_by_root.insert(101, vec![c4, c3, c2, c1]);
        let ctx = RenderCtx {
            slug: "/posts/hello-world/",
            locale: Locale::ZhCn,
            query_lang: None,
        };

        // Limit = 3 -> 1 root + 2 newest replies (104, 105), 2 older replies truncated.
        let pkg = build_export_package(
            &ctx,
            &roots_desc,
            &replies_desc_by_root,
            3,
            DEFAULT_EXPORT_MAX_BYTES,
        );
        assert_eq!(pkg.exported_count, 3);
        assert_eq!(pkg.last_cursor, 105);
        assert!(pkg.has_more);
        assert!(pkg.html.contains(r#"id="load-older-replies-101""#));
        assert!(pkg.html.contains("parent_id=101&amp;before=104"));
        assert!(pkg.html.contains(r#"data-i18n="load-older-replies-remaining" data-remaining="2""#));
        assert!(pkg.html.contains("还有 2 条"));
        assert!(!pkg.html.contains(r#"id="comment-102""#));
    }

    #[test]
    fn test_build_delta_html_mixed_oob_and_nested() {
        let rows = vec![
            make_row(
                105,
                Some(101),
                Some("Alice"),
                "Charlie",
                "<p>Reply to static root</p>",
                1732123600,
            ),
            make_row(106, None, None, "Dana", "<p>New root</p>", 1732123700),
            make_row(
                107,
                Some(106),
                Some("Dana"),
                "Eve",
                "<p>Reply to new root</p>",
                1732123800,
            ),
        ];
        let ctx = RenderCtx {
            slug: "/posts/hello-world/",
            locale: Locale::En,
            query_lang: None,
        };

        let html = build_delta_html(&ctx, &rows);
        assert!(html.contains(
            r#"<li id="comment-105" class="comment-reply-item" hx-swap-oob="beforeend:#replies-for-101">"#
        ));
        assert!(html.contains(r#"<li id="comment-106" class="comment-item" data-reply-count="1">"#));
        assert!(html.contains(r#"data-i18n="reply-count">1 reply</span>"#));
        assert!(html.contains(r#"data-i18n="reply""#));
        assert!(html.contains(r#"<li id="comment-107" class="comment-reply-item">"#));
        assert!(!html.contains(r#"hx-swap-oob="beforeend:#replies-for-106""#));
    }

    #[test]
    fn test_build_thread_replies_html_pagination() {
        let rows = vec![
            make_row(102, Some(101), Some("Alice"), "Bob", "<p>Old</p>", 1732123500),
            make_row(103, Some(101), Some("Bob"), "Carol", "<p>Mid</p>", 1732123550),
        ];
        let ctx = RenderCtx {
            slug: "/posts/hello-world/",
            locale: Locale::En,
            query_lang: Some(Locale::Ja),
        };
        let older = build_thread_replies_html(&ctx, 101, &rows, true, false);
        assert!(older.starts_with(r#"<li id="load-older-replies-101""#));
        assert!(older.contains("parent_id=101&amp;before=102&amp;lang=ja"));

        let newer = build_thread_replies_html(&ctx, 101, &rows, false, true);
        assert!(newer.ends_with(r##"<li id="load-newer-replies-101" class="load-more-replies"><button type="button" class="load-replies-btn" data-i18n="load-newer-replies" hx-get="/api/comments?slug=%2Fposts%2Fhello-world%2F&amp;parent_id=101&amp;after=103&amp;lang=ja" hx-target="#load-newer-replies-101" hx-swap="outerHTML">Load more replies</button></li>"##));
    }

    #[test]
    fn test_locale_parsing_and_accept_language() {
        assert_eq!(Locale::parse("en-US"), Some(Locale::En));
        assert_eq!(Locale::parse("zh_CN"), Some(Locale::ZhCn));
        assert_eq!(Locale::parse("zh-Hans"), Some(Locale::ZhCn));
        assert_eq!(Locale::parse("zh-TW"), Some(Locale::ZhTw));
        assert_eq!(Locale::parse("zh-HK"), Some(Locale::ZhTw));
        assert_eq!(Locale::parse("ja-JP"), Some(Locale::Ja));
        assert_eq!(Locale::parse("fr-FR"), None);

        assert_eq!(
            Locale::from_accept_language("fr-CH, ja;q=0.8, en-US;q=0.9"),
            Some(Locale::En)
        );
        assert_eq!(
            Locale::from_accept_language("zh-TW, zh-CN;q=0.8"),
            Some(Locale::ZhTw)
        );
        assert_eq!(Locale::from_accept_language("de-DE;q=0.9"), None);
    }

    #[test]
    fn test_is_allowed_loopback_origin() {
        assert!(is_allowed_loopback_origin("http://localhost:1313"));
        assert!(is_allowed_loopback_origin("http://127.0.0.1:8787"));
        assert!(is_allowed_loopback_origin("http://localhost"));
        assert!(is_allowed_loopback_origin("http://[::1]:1313"));
        assert!(!is_allowed_loopback_origin("https://localhost:1313"));
        assert!(!is_allowed_loopback_origin("https://evil.example.com"));
        assert!(!is_allowed_loopback_origin("http://localhost:1313/path"));
        assert!(!is_allowed_loopback_origin("not-a-url"));
    }
}
