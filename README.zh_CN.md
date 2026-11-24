# hx-echo

[English](./README.md) | [简体中文](./README.zh_CN.md)

基于 **Rust (WebAssembly)** 构建、运行在 **Cloudflare Workers & D1** 上的轻量级 HTMX 静态站点评论系统，内置 **Telegram Bot 双向通知与审核回复闭环**（不在网页端暴露任何 `admin_token` 表单）。

## 工作原理（静态基线 + 动态增量）

`hx-echo` 将评论拆分为**构建期静态基线（Base）**和**运行期动态增量（Delta）**：

1. **构建期静态基线（边缘缓存）**：静态站点生成（如 Hugo `resources.GetRemote`）时，请求 `GET /api/comments/export?slug=/posts/hello-world/&lang=zh-cn`，将历史评论和 `lastCursor` 直接编入静态 HTML（0ms 首屏可见，天然利于 SEO）。响应携带 `bakedAt` 时间戳与 `Cache-Control: public, max-age=60`（结合 Cloudflare Cache API 缓存），可吸收短时间内反复构建引发的爆发式全量导出请求；评论新增/隐藏后无需主动清缓存，因为运行期 Delta 接口会自动补齐最新评论。
2. **运行期动态增量（HTMX）**：当评论区滚动进入视口（`hx-trigger="intersect once"`）或页面加载时，HTMX 请求 `GET /api/comments?slug=/posts/hello-world/&after=<lastCursor>`。
   - 若无新增评论，Worker 返回 `204 No Content` 并附带 `Cache-Control: no-store`（零 DOM 改动）。
   - 新增顶级评论直接追加至 `#comment-list`；对已有静态主评论的回复通过 `hx-swap-oob="beforeend:#replies-for-<parent_id>"` 精确插入对应子列表，并同步通过 OOB 更新 `#reply-count-<parent_id>` 回复计数。
3. **发表评论与边缘防护**：通过 `hx-post="/api/comments"` 提交表单后，依次执行可选按 IP 边缘限流（`COMMENT_RATE_LIMITER`，触发返回 `429` 纯文本）、隐藏 `website` 蜜罐校验、保留博主昵称 `ADMIN_NAMES` 防冒充检查（命中返回 `403` 纯文本）、Markdown 渲染（`pulldown-cmark`）与 XSS 清洗（`ammonia`），写入 D1 后通过 `ctx.wait_until(...)` 异步推送 Telegram 通知，并返回渲染好的 `<li>` 片段。
4. **Telegram 双向管理闭环**：博主在 Telegram 中直接引用通知消息回复即可发布带 `.badge-admin` 标识的博主回复（`is_admin = 1`），也可回复 `/hide`、`/del` 隐藏评论、`/show` 恢复显示，或使用 `/post <slug> <内容>` 主动在指定文章下发表顶级评论。
5. **多域名同源与本地联调支持**：通过 Cloudflare Routes 将 Worker 挂载到多个镜像域名的 `<domain>/api/comments*`（含根域名与 `www.`），生产环境全同源零 CORS 预检；同时自动为本地回环来源（`http://localhost:*`、`http://127.0.0.1:*`）及环境变量 `CORS_ALLOW_ORIGINS` 提供完整 CORS 与 `HX-*` 响应头暴露。所有文章标识统一标准化为路径部分（`RelPermalink`）。

完整架构说明与 Hugo 接入模板请参阅 [ARCHITECTURE.md](./ARCHITECTURE.md)。

## 快速开始

### 方式一：GitHub Actions 引导式一键部署（推荐）

1. Fork 本仓库。
2. 在 GitHub 仓库的 **Settings → Secrets and variables → Actions → Secrets** 中配置：
   - **必填**：`CLOUDFLARE_API_TOKEN`（需具备 Workers 与 D1 编辑权限）和 `CLOUDFLARE_ACCOUNT_ID`。
   - **可选（启用 Telegram 闭环时填写）**：`TELEGRAM_BOT_TOKEN`、`TELEGRAM_WEBHOOK_SECRET`（以及可选的 `TELEGRAM_CHAT_ID`）。
3. 进入仓库 **Actions** 页面 → 选择 **Deploy hx-echo** → 点击 **Run workflow**，按界面表单填写：
   - 必填/核心项：`worker_name`、绑定的域名 `domains`（逗号分隔，留空则仅使用 `*.workers.dev`）、`api_path`（默认 `/api/comments*`）、`d1_database_name` / 可选 `d1_database_id`（留空会自动查找或新建 D1）。
   - 可选项：`default_lang`、`site_origin`、`admin_names`、`admin_display_name`、`telegram_chat_id`、`enable_rate_limit`、`export_cache_max_age`。
   - 工作流会自动在 Runner 中生成 `wrangler.toml`、按顺序执行 `migrations/` 数据库迁移、同步 Secrets、部署 Worker，并在配置完整时自动调用 Telegram `setWebhook`。
   - 如需后续 `push` 到 `main`/`master` 自动重新部署，可将生成的配置存入 `WRANGLER_TOML` Secret，或配置对应的 `HX_ECHO_*` Repository Variables。

### 方式二：本地 CLI 调试与手动部署

#### 1. 前置依赖

- [Rust](https://www.rust-lang.org/tools/install) 及 WebAssembly 编译目标：
  ```sh
  rustup target add wasm32-unknown-unknown
  ```
- [Node.js](https://nodejs.org/)（用于运行 `npx wrangler`，若启用 `[[ratelimits]]` 需 Wrangler `>= 4.36.0`）以及 Cloudflare 账号

#### 2. 配置 Wrangler 与 D1 Migration

从模板创建本地 `wrangler.toml`（`wrangler.toml` 已加入 `.gitignore`，避免个人数据库 ID 与域名泄露）：

```sh
cp wrangler.toml.example wrangler.toml
```

创建 Cloudflare D1 数据库：

```sh
npx wrangler d1 create hx-echo-db
```

将返回的 `database_id` 和域名 `routes` 填入 `wrangler.toml`，然后按顺序执行 `migrations/` 目录下的版本化迁移（`0001_init.sql` → `0002_tg_admin.sql`）：

```sh
# 本地开发数据库
npx wrangler d1 migrations apply DB --local

# 远程生产数据库（全新建库与已有旧库升级均适用）
npx wrangler d1 migrations apply DB --remote
```

*（注：`schema.sql` 作为最新完整表结构快照保留，便于对照查阅。）*

#### 3. 配置 Telegram Bot 与 Secrets（可选）

使用 Wrangler Secrets 写入敏感凭据（切勿将 Token 明文写入 `wrangler.toml`）：

```sh
npx wrangler secret put TELEGRAM_BOT_TOKEN
npx wrangler secret put TELEGRAM_WEBHOOK_SECRET
```

随后运行以下一行 `curl` 命令绑定 Webhook（替换 `<BOT_TOKEN>`、`<YOUR_DOMAIN>` 和 `<WEBHOOK_SECRET>`）：

```sh
curl -X POST "https://api.telegram.org/bot<BOT_TOKEN>/setWebhook" -H "Content-Type: application/json" -d '{"url":"https://<YOUR_DOMAIN>/api/comments/tg-webhook","secret_token":"<WEBHOOK_SECRET>"}'
```

#### 4. 本地调试与部署

```sh
# 运行单元测试
cargo test

# 启动本地 Worker 开发服务 (http://127.0.0.1:8787)
npx wrangler dev

# 编译并部署到 Cloudflare Workers
npx wrangler deploy
```

## 可选边缘限流保护（Rate Limiting）

为防止恶意刷屏消耗 D1 写入配额，可在 `wrangler.toml` 中取消注释启用 `[[ratelimits]]` 绑定：

```toml
[[ratelimits]]
name = "COMMENT_RATE_LIMITER"
namespace_id = "1001"

  [ratelimits.simple]
  limit = 5
  period = 60
```

- **可选设计**：未配置 `[[ratelimits]]` 绑定（或本地 `wrangler dev` 无 `CF-Connecting-IP` 请求头）时，Worker 会安全跳过限流检查，不会报错。
- **平台约束**：Cloudflare Workers Rate Limiting 要求 Wrangler `>= 4.36.0`，且 `period` 仅支持 `10` 或 `60` 秒。触发限流时，`POST /api/comments` 会返回 `429 Too Many Requests`（含 `Retry-After: 60` 与对应语种的纯文本提示）。
- `POST /api/comments/tg-webhook` 由 Telegram 服务器调用，不受访客 IP 限流限制。

## API 接口

| 方法 | 路径 | 参数 / 鉴权 | 响应 |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/&limit=20&max_bytes=32768&lang=zh-cn` | `200 OK` JSON `{ "lastCursor", "firstCursor", "hasMore", "exportedCount", "htmlBytes", "bakedAt", "html" }`（带 `Cache-Control: public, max-age=60`） |
| `GET` | `/api/comments` | `?slug=...&after=<id>`（增量）/ `&before=<id>&limit=20`（更早主评）/ `&parent_id=<id>&before\|after=<id>`（楼内回复），可选 `&lang=` | `204 No Content`（无增量）或 `200 OK` `text/html` 片段（均带 `Cache-Control: no-store`） |
| `POST` | `/api/comments` | 表单：`slug`、`author`、`content`、`parent_id?`、`reply_to_author?`、`lang?`、`website` | `200 OK` `text/html` `<li>`（冒充保留昵称返回 `403`，限流返回 `429`） |
| `POST` | `/api/comments/tg-webhook` | 请求头 `X-Telegram-Bot-Api-Secret-Token`；校验 `TELEGRAM_CHAT_ID` | `200 OK`（处理引用回复发布、`/hide`、`/del`、`/show`、`/post <slug> <内容>`） |

### 环境变量与 Secrets 说明

- `DEFAULT_LANG`（默认 `"en"`）：未传入 `?lang=`、`HX-Lang` 或 `Accept-Language` 时的默认界面语言（内置 `en`、`zh-cn`、`zh-tw`、`ja`）。
- `EXPORT_DEFAULT_LIMIT`（默认 `"20"`）、`EXPORT_MAX_LIMIT`（默认 `"100"`）、`EXPORT_MAX_BYTES`（默认 `"32768"`）。
- `EXPORT_CACHE_MAX_AGE`（默认 `"60"`）：`/api/comments/export` 的边缘缓存时长（秒），设为 `"0"` 可禁用缓存。
- `ADMIN_NAMES`：逗号分隔的保留博主昵称，网页端提交命中直接返回 `403` 防冒充。
- `ADMIN_DISPLAY_NAME`（默认 `"Admin"`）：通过 Telegram 回复或发帖时使用的博主显示名称（也会自动加入保留列表）。
- `SITE_ORIGIN`：站点根域名（如 `https://koa.im`），用于 Telegram 通知中生成可点击文章链接。
- `TELEGRAM_CHAT_ID`：授权接收通知及执行管理指令的 Telegram Chat ID（可放 `[vars]` 或 Secret）。
- `TELEGRAM_BOT_TOKEN` 与 `TELEGRAM_WEBHOOK_SECRET`：通过 `wrangler secret put` 配置的敏感凭据。

### 多语言国际化（i18n）

`hx-echo` 同时支持**服务端语言渲染**与**客户端自定义覆盖**：
- **服务端内置语种**：默认输出英文（`en`），内置 `en`、`zh-cn`、`zh-tw`、`ja`，解析优先级为：`lang`（表单/查询参数） > `HX-Lang` 请求头 > `Accept-Language` 请求头 > `DEFAULT_LANG` 环境变量 > `en`。显式传入的 `?lang=` 会自动透传到分页按钮的 `hx-get` URL 中。
- **客户端覆盖能力**：所有渲染出的交互按钮、徽章与计数均携带语义属性（`data-i18n="admin-badge"`、`data-i18n="reply"`、`data-i18n="reply-count"` 配合 `data-count`、`data-i18n="load-older-comments"`、`data-i18n="load-older-replies-remaining"` 配合 `data-remaining`、`data-i18n="load-replies-total"` 配合 `data-total`、`data-i18n="load-older-replies"`、`data-i18n="load-newer-replies"`），前端模板可在 `htmx:afterSettle` 时按任意语种或个性化话术覆盖。

## 前端接入（Hugo + HTMX）

1. 在静态站点 `<head>` 中引入 `htmx.min.js`（建议使用 `<script defer ...>`，避免 `async` 触发初始化竞态）。
2. 参考 [ARCHITECTURE.md](./ARCHITECTURE.md#6-hugo-partial-reference-layoutspartialscommentshtml) 接入评论模板，并在 `hugo.toml` 中配置 `hxEchoExportBase`（生产导出地址）与 `hxEchoLocalBase`（本地 `127.0.0.1:8787` 联调地址）。

## 开源协议

[MIT](./LICENSE)
