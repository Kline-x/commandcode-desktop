# ARCHITECTURE.md — 模块结构与数据流

## 1. 进程与线程模型

单进程（Tauri 主进程）。Tokio 运行时承载：

- **axum 代理服务**：监听 `127.0.0.1:3050`（可配）。每个请求一个 task。
- **控制 API**：监听随机端口，随机 token 鉴权，仅 WebView 使用。
- **配额轮询**：单个后台 task，45s 周期，串行探测各账号（避免惊群）。
- **Tauri 事件循环**：窗口/托盘/IPC。

共享状态用 `Arc<AppState>`，内部可变部分用 `tokio::sync::RwLock` / `DashMap`。
**账号池的轮换状态以 API key 为键**（进程内），与数据库分离：DB 存配置与快照，内存存瞬时状态。

## 2. 模块职责

```
main.rs / lib.rs      Tauri builder、插件注册、服务启动编排
proxy/server.rs       axum 路由与监听
proxy/state.rs        AppState：配置、池、存储、事件总线
proxy/routes/*        对外协议面（chat / messages / responses / models）
proxy/upstream/*      上游协议：请求构造、SSE 解析、指纹、会话
proxy/anthropic/*     Anthropic 形状的双向转换
pool.rs               账号池：解析、选择、轮换、探测复活
quota.rs              /alpha/* 轮询与解析 → 写库 + 推送事件
store/*               SQLite schema、迁移、CRUD
control/*             控制 API 与 SSE 事件总线
secrets.rs            stronghold 封装（加解密 API key）
tray.rs, commands.rs  托盘菜单、Tauri command
error.rs              统一错误类型与 HTTP 映射
```

## 3. 一次请求的生命周期

```
客户端 ──POST /v1/chat/completions──▶ routes/chat.rs
  1. 解析请求体（serde Value，保留未知字段）
  2. pool.resolve_key(model)
       ├─ 命中 model→account 路由规则？
       ├─ 否则 active_account 可用？
       └─ 否则轮转顺序中第一个可用账号
  3. upstream/convert.rs：OpenAI 形状 → /alpha/generate 形状
  4. upstream/forward.rs：注入伪装头，POST 上游（带 abort signal）
  5. 若 pre-stream 429/401/402：
       pool.mark_rejected(key, kind) → 回到 2（每 key 仅一次，上限 16）
     若 403 upgrade_required：切 /alpha/generate，记忆 15 分钟，重试同 key
  6. upstream/sse.rs：事件流 → 内部流块 → OpenAI SSE
  7. 请求结束：写 requests 表，向控制面推 request 事件
```

## 4. 状态所有权

| 状态 | 位置 | 生命周期 |
|---|---|---|
| 账号配置、key（密文） | SQLite | 持久 |
| 配额快照 | SQLite + 内存缓存（TTL 45s） | 持久 + 瞬时 |
| 轮换标记（cooldown / disabled） | **仅内存**，按 key | 进程内；重启后由探测重建 |
| 协议偏好（cli / openai） | 内存，按 key，TTL 15min | 进程内 |
| 指纹 / sessionId | 内存，按 key | 进程内，自动刷新 |
| 请求流水 | SQLite（可配置保留期） | 持久 |

> 轮换标记刻意**不持久化**：过期的标记会让重启后的账号被无谓跳过；上游窗口状态本来就该由探测确定。

## 5. 事件总线（控制面 → UI）

```
request  { ts, account_id, model, status, input_tokens, output_tokens,
           cached_tokens, ttft_ms, total_ms, retries }
quota    { account_id, win5, weekly, monthly, credits, alerts[] }
account  { id, label, enabled, mark }     // 增删改 / 状态变化
log      { level, msg, ctx }
```

UI 通过 `GET /events`（SSE）订阅。**请求级**事件实时推送；**配额级**由 45s 轮询产生——
上游本身不是实时数据源，不要为此加密轮询。

## 6. 错误处理约定

- 所有对外错误映射为 OpenAI / Anthropic 的**标准错误信封**（路由决定用哪个）；
- 保留 `retry_after`，让客户端 SDK 拿到正确退避提示；
- 内部错误携带稳定的 `error_code`，用于前端分支与日志检索；
- 上游原始错误文本截断保存（默认 500 字符），不整段回传。

## 7. 安全边界

| 面 | 暴露范围 | 保护 |
|---|---|---|
| 代理端口 | 默认仅 127.0.0.1，可显式开到 LAN | 可选 `proxy_token` |
| 控制 API | 仅回环 + 随机端口 | 每次启动随机 `x-control-token`，仅注入 WebView |
| API key | 磁盘密文 | stronghold（argon2 派生），日志一律脱敏为 `user_…xxxx` |
| 遥测 | 无 | 不发送任何统计 |
