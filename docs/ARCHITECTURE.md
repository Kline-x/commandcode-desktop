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
bootstrap.rs          接线：打开存储、起控制面与代理面、把配额探测结果反馈给账号池
proxy.rs              axum 路由、三协议入口、轮换循环、SSE 出流
config.rs             连接级配置（上游基址、超时、请求体上限、协议偏好）
error.rs              统一错误类型与 HTTP 映射；**轮换语义的判定依据**
pool.rs               账号池：解析、选择、轮换、探测复活
quota.rs              /alpha/* 响应解析 → QuotaSnapshot（纯函数，无 I/O）
quota_poller.rs       周期性拉取配额并广播快照（网络 + 调度）
convert.rs            OpenAI 形状 → /alpha/generate 形状
anthropic.rs          Anthropic Messages ↔ 上游的双向转换
responses.rs          OpenAI Responses API ↔ 内部 Chat 格式
openai.rs             上游事件 → OpenAI SSE 的有状态转换
sse.rs                上游 NDJSON/SSE 行解析
upstream.rs           上游 HTTP 客户端：伪装头、会话/指纹、请求发送与事件流
store.rs              SQLite schema、迁移、CRUD
control.rs            控制 API 与内存日志缓冲区
secrets.rs            keyring/本地文件主密钥 + AES-256-GCM（加解密 API key）
tray.rs, commands.rs  托盘菜单、Tauri command
mock_upstream.rs      测试用 mock 上游（真实 axum 服务器，feature 门控）
```

> 实现细节：模块**平铺**在 `crates/cc-server/src/` 下，没有 `proxy/routes/*` 这类子目录。
> 理由见第 4.5 节的偏差说明（模块数量与耦合度还不需要目录层级）。

## 3. 一次请求的生命周期

```
客户端 ──POST /v1/chat/completions──▶ proxy.rs
  1. 解析请求体（serde Value，保留未知字段；上限见 Config::max_body_bytes）
  2. 从池中选号（模型路由 → 手动指定 → 轮转首个可用）
  3. convert.rs：OpenAI 形状 → /alpha/generate 形状
  4. upstream.rs：注入伪装头，POST 上游（带 abort signal）
  5. 若 pre-stream 401/402/429（且 rotates_account() 为真）：
       pool.mark_rejected(key, kind) → 换号重试（每 key 仅一次，上限 16）
      ⚠️ 标记**只在**该错误确实与账号相关时发生；403「模型不在套餐」是请求侧
         错误，换号无用，标记会误伤账号池（见 PLAN.md 8.1 与 4.6）。
     若 403 upgrade_required：切 /alpha/generate，记忆 15 分钟，重试同 key
  6. sse.rs + openai.rs / anthropic.rs / responses.rs：事件流 → 对应协议 SSE
  7. 请求结束：写 requests 表，向控制面推 request 事件
```

## 3.1 账号的复活路径（429 之后如何恢复）

被 429 标记的账号**不会**在进程内自动恢复，除非配额轮询把「窗口已重置」
这一事实反馈给池：

```
QuotaPoller（45s）──QuotaSnapshot──▶ bootstrap 的订阅循环
                                        ├─ store.update_account_quota（写库，供面板）
                                        └─ ProxyState::apply_probe_for_slot
                                              └─ window_probe_from_snapshot
                                                    ├─ 窗口未超限 → 清除标记（复活）
                                                    ├─ 窗口仍超限 → 记录 cooldown + 重置时刻
                                                    └─ 无窗口数据 → 不改状态（探测失败无信息）
```

**这条链路是必需的**：`AccountPool::apply_probe` 是唯一的清除路径，
漏接它就等于「账号一旦 429 就永久不可用，只能重启进程」——
而给用户的错误文案恰恰承诺「窗口重置后请求会自动恢复」。

## 4. 状态所有权

| 状态 | 位置 | 生命周期 |
|---|---|---|
| 账号配置、key（密文） | SQLite | 持久 |
| 配额快照 | SQLite + 内存缓存（TTL 45s） | 持久 + 瞬时 |
| 轮换标记（cooldown / disabled） | **仅内存**，按 key | 进程内；由 45s 配额探测清除/重建（见 3.1） |
| 协议偏好（cli / openai） | 内存，按 key，TTL 15min | 进程内 |
| 指纹 / sessionId | 内存，按 key | 进程内，自动刷新 |
| 请求流水 | SQLite（保留条数可在设置页调整） | 持久 |

> 轮换标记刻意**不持久化**：过期的标记会让重启后的账号被无谓跳过；上游窗口状态本来就该由探测确定。
> 但「由探测确定」意味着**探测必须真的接回池**——这正是 3.1 那条链路的职责。

## 5. 控制面与 UI 的数据流

```
UI（WebView）──2s 轮询──▶ POST /api/requests/recent · /api/accounts/list
                          GET  /api/logs（2.5s）· /api/settings · /api/rules
```

UI 侧为**轮询**而非 SSE 订阅。原设计（`GET /events` 事件总线：request / quota /
account / log）未实现——本地回环调用成本可忽略，轮询实现更简单且没有长连接
状态要维护（见 PLAN.md Phase 3 的备注）。若将来请求量增大到 2s 轮询有感知，
再补 SSE 推送。

## 6. 错误处理约定

- 所有对外错误映射为 OpenAI / Anthropic 的**标准错误信封**（路由决定用哪个）；
- 保留 `retry_after`，让客户端 SDK 拿到正确退避提示；
- 内部错误携带稳定的 `error_code`，用于前端分支与日志检索；
- 上游原始错误文本截断保存（默认 500 字符），不整段回传。

## 7. 安全边界

| 面 | 暴露范围 | 保护 |
|---|---|---|
| 代理端口 | 默认仅 127.0.0.1（硬编码 3050） | 无 token：本地客户端需能直接调用 |
| 控制 API | 仅回环 + 随机端口 | 每次启动随机 `x-control-token`（定长比较），仅注入 WebView |
| API key | 磁盘密文（AES-256-GCM，随机 nonce） | 主密钥优先取本地 `0600` 文件，兼容旧版系统钥匙串；日志脱敏为 `user_…xxxx` |
| 遥测 | 无 | 不发送任何统计 |

> 代理面**没有** token 鉴权（原计划的 `proxy_token` 未实现）：它只监听回环，
> 且本机任意进程本来就能直接调用它。若将来要显式开到 LAN（`Config::listen_addr`
> 目前亦未被 bootstrap 使用），必须同时补上鉴权。
