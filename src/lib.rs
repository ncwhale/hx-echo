use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use ammonia::clean;
use pulldown_cmark::{html::push_html, Options, Parser};
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::JsValue;
use worker::*;

const MAX_AUTHOR_CHARS: usize = 50;
const MAX_CONTENT_CHARS: usize = 2500;

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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportResponse {
    last_cursor: i64,
    html: String,
}

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    Router::new()
        .get_async("/api/comments/export", handle_export)
        .get_async("/api/comments", handle_get_comments)
        .post_async("/api/comments", handle_post_comment)
        .run(req, env)
        .await
}

/// `GET /api/comments/export?slug=/posts/xxx/`
/// Returns `{ "lastCursor": i64, "html": String }` for Hugo build-time embedding.
async fn handle_export(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let url = req.url()?;
    let Some(slug) = query_param(&url, "slug").and_then(|s| normalize_slug(&s)) else {
        return Response::error("Missing or invalid 'slug' query parameter", 400);
    };

    let db = ctx.env.d1("DB")?;
    let stmt = db.prepare(
        "SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status \
         FROM comments \
         WHERE slug = ?1 AND status = 1 \
         ORDER BY id ASC",
    );
    let result = stmt.bind(&[JsValue::from_str(&slug)])?.all().await?;
    let rows: Vec<CommentRow> = result.results()?;

    let (last_cursor, html) = build_comment_tree_html(&rows);
    Response::from_json(&ExportResponse { last_cursor, html })
}

/// `GET /api/comments?slug=/posts/xxx/&after=101`
/// Returns `204 No Content` if no newer comments exist, or HTML fragments for HTMX.
async fn handle_get_comments(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let url = req.url()?;
    let Some(slug) = query_param(&url, "slug").and_then(|s| normalize_slug(&s)) else {
        return Response::error("Missing or invalid 'slug' query parameter", 400);
    };

    let after: i64 = match query_param(&url, "after") {
        Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<i64>() {
            Ok(val) if val >= 0 => val,
            _ => return Response::error("Invalid 'after' cursor", 400),
        },
        _ => 0,
    };

    let db = ctx.env.d1("DB")?;
    let stmt = db.prepare(
        "SELECT id, slug, parent_id, reply_to_author, author, content_html, created_at, status \
         FROM comments \
         WHERE slug = ?1 AND status = 1 AND id > ?2 \
         ORDER BY id ASC",
    );
    let result = stmt
        .bind(&[JsValue::from_str(&slug), JsValue::from_f64(after as f64)])?
        .all()
        .await?;
    let rows: Vec<CommentRow> = result.results()?;

    if rows.is_empty() {
        return empty_no_content();
    }

    let html = build_delta_html(&rows);
    if html.is_empty() {
        return empty_no_content();
    }

    Response::from_html(html)
}

/// `POST /api/comments`
/// Validates honeypot, sanitizes Markdown, persists to D1, and returns the rendered `<li>`.
async fn handle_post_comment(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
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
             RETURNING id, slug, parent_id, reply_to_author, author, content_html, created_at, status",
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

    let comment = inserted.ok_or_else(|| Error::RustError("Failed to insert comment".into()))?;

    let html = match comment.parent_id {
        Some(root_id) => render_reply_item(&comment, root_id, false),
        None => render_comment_node(&comment, &[]),
    };

    let mut response = Response::from_html(html)?;
    if let Some(root_id) = comment.parent_id {
        let headers = response.headers_mut();
        headers.set("HX-Retarget", &format!("#replies-for-{}", root_id))?;
        headers.set("HX-Reswap", "beforeend")?;
    }
    Ok(response)
}

fn empty_no_content() -> Result<Response> {
    Ok(Response::empty()?.with_status(204))
}

fn query_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
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

/// Builds the full two-level nested comment HTML and returns `(last_cursor, html)`.
fn build_comment_tree_html(rows: &[CommentRow]) -> (i64, String) {
    let mut last_cursor = 0_i64;
    let mut roots: Vec<&CommentRow> = Vec::new();
    let mut children_map: BTreeMap<i64, Vec<&CommentRow>> = BTreeMap::new();

    for row in rows {
        if row.id > last_cursor {
            last_cursor = row.id;
        }
        match row.parent_id {
            None => roots.push(row),
            Some(pid) => children_map.entry(pid).or_default().push(row),
        }
    }

    let mut html = String::new();
    for root in roots {
        let children = children_map
            .get(&root.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        html.push_str(&render_comment_node(root, children));
    }

    (last_cursor, html)
}

/// Builds incremental HTMX fragments for comments newer than `after`.
/// Replies to new top-level comments in the same batch are nested directly;
/// replies to existing static comments carry `hx-swap-oob="beforeend:#replies-for-{parent_id}"`.
fn build_delta_html(rows: &[CommentRow]) -> String {
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
        html.push_str(&render_comment_node(root, children));
    }
    for reply in oob_replies {
        if let Some(root_id) = reply.parent_id {
            html.push_str(&render_reply_item(reply, root_id, true));
        }
    }

    html
}

fn render_comment_node(comment: &CommentRow, children: &[&CommentRow]) -> String {
    let author_html = escape_html(&comment.author);
    let author_js = escape_js_single_quoted(&comment.author);

    let mut out = String::new();
    let _ = write!(
        out,
        r#"<li id="comment-{id}" class="comment-item"><div class="comment-meta"><strong>{author}</strong><time data-ts="{ts}">{ts}</time><button type="button" class="reply-btn" onclick="setReplyTarget({id}, '{author_js}')">回复</button></div><div class="comment-body">{body}</div><ul id="replies-for-{id}" class="comment-replies">"#,
        id = comment.id,
        author = author_html,
        ts = comment.created_at,
        author_js = author_js,
        body = comment.content_html,
    );

    for child in children {
        out.push_str(&render_reply_item(child, comment.id, false));
    }

    out.push_str("</ul></li>");
    out
}

fn render_reply_item(reply: &CommentRow, root_id: i64, oob: bool) -> String {
    let author_html = escape_html(&reply.author);
    let author_js = escape_js_single_quoted(&reply.author);

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
        r#"<li id="comment-{id}" class="comment-reply-item"{oob}><div class="comment-meta"><strong>{author}</strong><time data-ts="{ts}">{ts}</time><button type="button" class="reply-btn" onclick="setReplyTarget({root_id}, '{author_js}')">回复</button></div><div class="comment-body">{prefix}{body}</div></li>"#,
        id = reply.id,
        oob = oob_attr,
        author = author_html,
        ts = reply.created_at,
        root_id = root_id,
        author_js = author_js,
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
    fn test_build_comment_tree_html() {
        let rows = vec![
            make_row(101, None, None, "Alice", "<p>Hello world!</p>", 1732123456),
            make_row(
                102,
                Some(101),
                Some("Alice"),
                "Bob",
                "<p>Welcome!</p>",
                1732123500,
            ),
        ];

        let (cursor, html) = build_comment_tree_html(&rows);
        assert_eq!(cursor, 102);
        assert!(html.contains(r#"<li id="comment-101" class="comment-item">"#));
        assert!(html.contains(r#"onclick="setReplyTarget(101, 'Alice')""#));
        assert!(html.contains(r#"<ul id="replies-for-101" class="comment-replies">"#));
        assert!(html.contains(r#"<li id="comment-102" class="comment-reply-item">"#));
        assert!(html.contains(r#"onclick="setReplyTarget(101, 'Bob')""#));
        assert!(html.contains(r#"<span class="reply-to">@Alice</span> <p>Welcome!</p>"#));
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

        let html = build_delta_html(&rows);
        assert!(html.contains(
            r#"<li id="comment-105" class="comment-reply-item" hx-swap-oob="beforeend:#replies-for-101">"#
        ));
        assert!(html.contains(r#"<li id="comment-106" class="comment-item">"#));
        assert!(html.contains(r#"<li id="comment-107" class="comment-reply-item">"#));
        assert!(!html.contains(r#"hx-swap-oob="beforeend:#replies-for-106""#));
    }
}
