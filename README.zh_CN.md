# hx-echo

[English](./README.md) | [简体中文](./README.zh_CN.md)

基于 **Rust (WebAssembly)** 构建、运行在 **Cloudflare Workers & D1** 上的轻量级 HTMX 静态站点评论系统。

## 工作原理（静态基线 + 动态增量）

`hx-echo` 将评论拆分为**构建期静态基线（Base）**和**运行期动态增量（Delta）**：

1. **构建期静态基线**：静态站点生成（如 Hugo `resources.GetRemote`）时，请求 `GET /api/comments/export?slug=/posts/hello-world/&lang=zh-cn`，将历史评论和 `lastCursor` 直接编入静态 HTML（0ms 首屏可见，天然利于 SEO）。
2. **运行期动态增量（HTMX）**：当评论区滚动进入视口（`hx-trigger="intersect once"`）或页面加载时，HTMX 请求 `GET /api/comments?slug=/posts/hello-world/&after=<lastCursor>`。
   - 若无新增评论，Worker 返回 `204 No Content` 并附带 `Cache-Control: no-store`（零 DOM 改动）。
   - 新增顶级评论直接追加至 `#comment-list`；对已有静态主评论的回复通过 `hx-swap-oob="beforeend:#replies-for-<parent_id>"` 精确插入对应子列表，并同步通过 OOB 更新 `#reply-count-<parent_id>` 回复计数。
3. **发表评论**：通过 `hx-post="/api/comments"` 提交表单后，在边缘节点完成 Markdown 渲染（`pulldown-cmark`）、XSS 清洗（`ammonia`）及隐藏 `website` 蜜罐反垃圾校验，写入 D1 后返回渲染好的 `<li>` 片段（回复自动附带 `HX-Retarget` / `HX-Reswap` 响应头及 OOB 回复数更新）。
4. **多域名同源与本地联调支持**：通过 Cloudflare Routes 将 Worker 挂载到多个镜像域名的 `<domain>/api/comments*`（含根域名与 `www.`），生产环境全同源零 CORS 预检；同时自动为本地回环来源（`http://localhost:*`、`http://127.0.0.1:*`）及环境变量 `CORS_ALLOW_ORIGINS` 提供完整 CORS 与 `HX-*` 响应头暴露。所有文章标识统一标准化为路径部分（`RelPermalink`）。

完整架构说明与 Hugo 接入模板请参阅 [ARCHITECTURE.md](./ARCHITECTURE.md)。

## 快速开始

### 1. 前置依赖

- [Rust](https://www.rust-lang.org/tools/install) 及 WebAssembly 编译目标：
  ```sh
  rustup target add wasm32-unknown-unknown
  ```
- [Node.js](https://nodejs.org/)（用于运行 `npx wrangler`）以及 Cloudflare 账号

### 2. 配置 Wrangler 与 D1

从模板创建本地 `wrangler.toml`（`wrangler.toml` 已加入 `.gitignore`，避免个人数据库 ID 与域名泄露）：

```sh
cp wrangler.toml.example wrangler.toml
```

创建 Cloudflare D1 数据库：

```sh
npx wrangler d1 create hx-echo-db
```

将返回的 `database_id` 和域名 `routes` 填入 `wrangler.toml`（如区分本地与生产，可使用 `[env.production]`；注意 Wrangler 中 `routes`、`d1_databases`、`vars` 为不可继承字段，需在 `[env.production]` 下单独配置），随后初始化表结构：

```sh
# 本地开发数据库
npx wrangler d1 execute hx-echo-db --local --file=./schema.sql

# 远程生产数据库
npx wrangler d1 execute hx-echo-db --remote --file=./schema.sql
```

### 3. 本地调试与部署

```sh
# 运行单元测试
cargo test

# 启动本地 Worker 开发服务 (http://127.0.0.1:8787)
npx wrangler dev

# 编译并部署到 Cloudflare Workers
npx wrangler deploy
# 若配置了 [env.production]：
npx wrangler deploy --env production
```

## API 接口

| 方法 | 路径 | 参数 | 响应 |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/comments/export` | `?slug=/posts/xxx/&limit=20&max_bytes=32768&lang=zh-cn` | `200 OK` JSON `{ "lastCursor": <id>, "firstCursor": <id>, "hasMore": bool, "exportedCount": n, "htmlBytes": n, "html": "..." }` |
| `GET` | `/api/comments` | `?slug=...&after=<id>`（增量）/ `&before=<id>&limit=20`（更早主评）/ `&parent_id=<id>&before\|after=<id>`（楼内回复），可选 `&lang=` | `204 No Content`（无增量）或 `200 OK` `text/html` 片段（均带 `Cache-Control: no-store`） |
| `POST` | `/api/comments` | 表单：`slug`、`author`、`content`、`parent_id?`、`reply_to_author?`、`lang?`、`website` | `200 OK` `text/html` `<li>`（若为回复，设置 `HX-Retarget` / `HX-Reswap` 与 OOB 回复计数） |

### 导出上限与字节熔断机制

管理员可在 `wrangler.toml` 的 `[vars]` 中配置：
- `DEFAULT_LANG`（默认 `"en"`）：未传入 `?lang=`、`HX-Lang` 或 `Accept-Language` 时的默认界面语言（内置 `en`、`zh-cn`、`zh-tw`、`ja`）。
- `EXPORT_DEFAULT_LIMIT`（默认 `"20"`）：每篇文章构建期默认导出的最大评论条数。
- `EXPORT_MAX_LIMIT`（默认 `"100"`）：查询参数 `?limit=` 允许的上限。
- `EXPORT_MAX_BYTES`（默认 `"32768"`，即 32 KiB）：导出 `html` 的默认及强制 UTF-8 字节上限（调用方可通过 `?max_bytes=` / `?maxBytes=` 进一步收紧，不可突破服务端上限）。

当导出历史评论或某条主评论下的回复被截断时，`hx-echo` 会自动生成按需加载的 HTMX 按钮（`#load-older-comments`、`#load-older-replies-<id>`、`#load-newer-replies-<id>`），供访客点击加载更早或更多回复。

### 多语言国际化（i18n）

`hx-echo` 同时支持**服务端语言渲染**与**客户端自定义覆盖**：
- **服务端内置语种**：默认输出英文（`en`），内置 `en`、`zh-cn`、`zh-tw`、`ja`，解析优先级为：`lang`（表单/查询参数） > `HX-Lang` 请求头 > `Accept-Language` 请求头 > `DEFAULT_LANG` 环境变量 > `en`。显式传入的 `?lang=` 会自动透传到分页按钮的 `hx-get` URL 中。
- **客户端覆盖能力**：所有渲染出的交互按钮与计数均携带语义属性（`data-i18n="reply"`、`data-i18n="reply-count"` 配合 `data-count`、`data-i18n="load-older-comments"`、`data-i18n="load-older-replies-remaining"` 配合 `data-remaining`、`data-i18n="load-replies-total"` 配合 `data-total`、`data-i18n="load-older-replies"`、`data-i18n="load-newer-replies"`），前端模板可在 `htmx:afterSettle` 时按任意小语种或个性化话术覆盖。

## 前端接入（Hugo + HTMX）

1. 在静态站点 `<head>` 中引入 `htmx.min.js`（建议使用 `<script defer ...>`，避免 `async` 触发初始化竞态）。
2. 参考 [ARCHITECTURE.md](./ARCHITECTURE.md#6-hugo-partial-reference-layoutspartialscommentshtml) 接入评论模板，并在 `hugo.toml` 中配置 `hxEchoExportBase`（生产导出地址）与 `hxEchoLocalBase`（本地 `127.0.0.1:8787` 联调地址）。

## 开源协议

[MIT](./LICENSE)
