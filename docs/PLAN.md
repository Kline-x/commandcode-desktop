# commandcode-desktop — 实施计划（Tauri 2 + Rust）

> 跨平台桌面客户端：多账号轮换代理（OpenAI / Anthropic 兼容）+ 实时额度用量面板。
> 本文是 Phase 0–5 的完整工程计划，作为仓库的单一事实来源（single source of truth）。

- 状态：**Phase 0 进行中**
- 技术栈：Tauri 2 + 纯 Rust 后端 + React/Vite 前端
- 目标平台：macOS (arm64/x64)、Windows (x64, 可选 arm64)、Linux (x64, 可选 arm64)
- 许可：MIT。协议知识来源于 MIT 上游项目，见 [THIRD_PARTY.md](../THIRD_PARTY.md)

---

## 1. 目标与非目标

### 交付物

一个双击即用的桌面 App：

1. **多账号代理** — 本地起 OpenAI / Anthropic 兼容端点，账号池自动轮换；某账号 429/401 时对客户端无感切换。
2. **实时用量** — 5 小时滚动 / 每周 / 每月额度进度、余额、逐请求流水（token、缓存命中、成本、TTFT）。
3. **账号与规则管理** — 增删改账号、模型→账号路由规则、启停。
4. **常驻形态** — 托盘后台运行；关闭窗口不停止代理。

### 非目标（明确排除，避免范围蔓延）

- 不做云端同步 / 多设备共享账号池
- 不做移动端
- 不做多用户、团队、权限体系
- 不做自有协议层（只做协议翻译，不与上游竞争）

---

## 2. 为什么是纯 Rust（决策记录）

上游有两个 MIT 姊妹项目提供了全部协议知识：

| 项目 | 作用 | 规模 |
|---|---|---|
| [MAXeaglet/commandcode-proxy](https://github.com/MAXeaglet/commandcode-proxy) | Command Code → OpenAI/Anthropic 反代 | 单文件 `proxy.mjs`，2956 行 |
| [MAXeaglet/commandcode-usage](https://github.com/MAXeaglet/commandcode-usage) | 多账号额度面板（Cloudflare Worker + D1） | `worker.js` + `public/index.html` |

**候选方案对比：**

| | Node sidecar | **纯 Rust（选定）** |
|---|---|---|
| 协议层 | 直接复用 2956 行 JS | 重写约 4000–5000 行 |
| 窗口/托盘/打包 | Rust | Rust（相同） |
| 单机产物 | App + ~100MB Node 运行时 | **单个 ~15MB 二进制** |
| 跨平台打包 | Node SEA **不支持交叉编译**，需三平台各自构建 + 各自签名 | `cargo build` 交给 CI 矩阵，无额外负担 |
| 进程管理 | 僵尸进程 / kill-tree / 子进程签名（Windows 尤甚） | **不存在该问题** |
| 上游同步 | 可 diff 上游 JS | 需手工跟（用 conformance 测试兜底） |

**关键依据**：`proxy.mjs` 的运行时依赖只有 `crypto / fs / http / path / url` 五个 Node 核心模块，**零 npm 依赖、零原生模块**，且 fingerprint 等"黑魔法"部分只是 `sha256` + 随机数拼装。因此 Rust 重写没有技术障碍，而 Node sidecar 在跨平台分发上的成本恰好抵消了它节省的移植时间。

**代价**：逆向协议的每次上游变更需要手工跟随 → 用第 6 节的双实现 conformance 测试把"跟上游"变成一次可执行的对比任务。

---

## 3. 架构

```
┌─ Tauri 2 App（单进程，单二进制）─────────────────────────────┐
│  Rust Core                                                    │
│   ├─ 窗口 / 托盘 / 单实例 / 开机自启 / 全局快捷键              │
│   ├─ 密钥：stronghold 加密存储                                 │
│   ├─ SQLite：账号 / 配额快照 / 请求流水 / 规则 / 设置           │
│   ├─ 代理服务（axum，127.0.0.1:3050 可配）                     │
│   │    /v1/chat/completions · /v1/messages                    │
│   │    /v1/responses · /v1/models · /health                   │
│   ├─ 账号池：resolve → 路由 → 轮换 → 探测复活                  │
│   ├─ 配额轮询（45s）：whoami / credits / subscriptions / summary│
│   └─ 控制 API（独立随机端口 + 启动随机 token）→ IPC/HTTP 给 UI  │
│  WebView (React + Vite)                                       │
└───────────────────────────────────────────────────────────────┘
                          │
                          ▼  https://api.commandcode.ai
     /alpha/generate（主，逆向下游可用） · /alpha/*（配额） · /provider/v1/*（Pro+）
```

**安全设计（易被忽略）**：代理端口按需暴露（默认仅 127.0.0.1）；**控制 API 使用随机端口 + 每次启动生成的随机 token**，仅 Tauri 前端知晓。否则本机任意网页都能通过 loopback 读取账号列表与 key 状态。

---

## 4. 技术选型

| 用途 | 选型 | 说明 |
|---|---|---|
| 桌面外壳 | **Tauri 2** | 体积小，Rust 原生能力完整 |
| HTTP 服务 | **axum** | 与 tokio 生态一致，SSE 支持好 |
| 上游请求 | **reqwest**（stream） | 复用连接池，支持 abort |
| SSE 解析 | **eventsource-stream** + `tokio_util::codec` | 手写状态机的兜底方案 |
| 序列化 | **serde / serde_json** | 协议转换层优先用 `Value`，避免过早强类型 |
| 哈希 / 随机 | **sha2** + **rand** | fingerprint、traceparent |
| 数据库 | **rusqlite**（`bundled` feature） | 自带 SQLite 源码，不依赖系统库 |
| 密钥 | **tauri-plugin-stronghold** | 纯 Rust，三平台行为一致；避免 Linux Secret Service 缺失问题 |
| 前端 | **React + Vite + TypeScript** | Tauri 默认模板 |
| 托盘 / 单实例 / 自启 | `tray-icon` / `tauri-plugin-single-instance` / `tauri-plugin-autostart` | 官方或社区成熟插件 |
| 更新 | `tauri-plugin-updater` | 已支持 deb/rpm/AppImage/NSIS/MSI |

---

## 5. 模块映射（JS → Rust）

```
proxy.mjs                              →  src-tauri/src/
  buildCcRequest / 消息转换              →  upstream/convert.rs
  createSseTranslator / makeChunk        →  upstream/sse.rs          ★核心
  convertAnthropicToOpenAI / 反向转换    →  anthropic/convert.rs
  handleChatCompletions                  →  routes/chat.rs
  handleMessages                         →  routes/messages.rs
  handleResponses                        →  routes/responses.rs
  generateFingerprint / 版本探测          →  upstream/fingerprint.rs
  ensureSession / keyStateStore          →  upstream/session.rs
  getApiKey / mapCcError / watchdog      →  upstream/util.rs
  ── 新增 ──
  账号池 + 轮换（移植 accounts.ts）        →  pool.rs
  配额轮询与解析                          →  quota.rs
  控制 API + SSE 推送                     →  control.rs
  SQLite                                  →  store.rs
```

```
worker.js / public/index.html          →  src/（React）
  账号卡片矩阵 / 三条进度条                →  pages/Dashboard.tsx
  月度 cap 派生 / 告警徽标                 →  lib/quota.ts（逻辑移植）
  详情抽屉 / 刷新                          →  components/AccountCard.tsx
```

---

## 6. 仓库结构与目录契约

```
commandcode-desktop/
├─ src-tauri/
│  ├─ src/
│  │  ├─ main.rs            # 入口、Tauri builder、插件注册
│  │  ├─ lib.rs             # run()
│  │  ├─ proxy/             # 代理服务（axum）
│  │  │  ├─ mod.rs  server.rs  state.rs
│  │  │  ├─ routes/{chat.rs, messages.rs, responses.rs, models.rs}
│  │  │  ├─ upstream/{mod.rs, convert.rs, sse.rs, fingerprint.rs, session.rs, util.rs}
│  │  │  └─ anthropic/{mod.rs, convert.rs, sse.rs}
│  │  ├─ pool.rs            # 账号池与轮换
│  │  ├─ quota.rs           # /alpha/* 配额轮询与解析
│  │  ├─ store/             # SQLite：mod.rs schema.rs migrate.rs
│  │  ├─ control/           # 控制 API：mod.rs api.rs events.rs
│  │  ├─ secrets.rs         # stronghold 封装
│  │  ├─ tray.rs  commands.rs
│  │  └─ error.rs
│  ├─ Cargo.toml
│  ├─ tauri.conf.json
│  └─ icons/
├─ src/                     # React 前端
│  ├─ pages/{Dashboard,Accounts,Requests,Rules,Settings,Logs}.tsx
│  ├─ components/{QuotaBar,AccountCard,RequestTable,LiveEventFeed}.tsx
│  └─ lib/{api.ts, sse.ts, quota.ts, format.ts}
├─ tests/                   # Rust 集成测试 + conformance
├─ third_party/             # vendored 上游 JS（当 oracle，不参与构建）
│  ├─ proxy.mjs
│  ├─ usage-worker.js
│  └─ upstream.json         # 记录 commit hash
├─ docs/{PLAN.md, PROTOCOL.md, ARCHITECTURE.md}
├─ scripts/mock-upstream.mjs
└─ .github/workflows/{ci.yml, release.yml}
```

---

## 7. 阶段划分

### Phase 0 — 骨架（1 天）

- [x] 建仓库、MIT LICENSE、.gitignore、README、THIRD_PARTY.md
- [x] 记录上游 commit hash 到 `third_party/upstream.json`
- [ ] Tauri 2 模板 + React/Vite 前端跑通空白窗口
- [ ] `axum` 起 `/health`，端口可配
- [ ] `scripts/mock-upstream.mjs`：可注入 401 / 402 / 429 / 403 / 正常流
- [ ] CI 矩阵骨架（三平台 `cargo check`）

**验收**：`pnpm tauri dev` 出窗口；`curl /health` 返回版本；CI 三平台通过。

### Phase 1 — 账号池 + 聊天链路（5–7 天）★核心

- [ ] `pool.rs`：`resolve_key(model)` / `mark_rejected` / `probe_revival` / 路由规则
- [ ] 轮换循环：仅 **pre-stream** 429/401 换 key；每 key 仅一次；硬上限 16
- [ ] `upstream/convert.rs` + `sse.rs`：`/v1/chat/completions` → `/alpha/generate` → SSE 回译
- [ ] `store`：accounts / requests / route_rules 建表与迁移
- [ ] 错误矩阵单测（见第 8 节）

**验收**：mock 上游连续返回 401→429→200 时客户端无感拿到 200；全池耗尽时返回带"最早重置时间"的错误；`pool/passthrough` 双模式可切。

### Phase 2 — Anthropic 面 + 配额（3 天）

- [ ] `/v1/messages`（Anthropic Messages 转换、thinking signature、`signature_delta`）
- [ ] `/v1/responses` 与 `/v1/models`
- [ ] `quota.rs`：四端点轮询 + 容错解析 + 月度 cap 派生
- [ ] `third_party/usage-worker.js` 的解析逻辑以 Rust 单测固化

**验收**：Anthropic SDK 客户端可直连；配额面板数据与网页版面板一致；字段缺失时降级不 panic。

### Phase 3 — 控制 API + 面板（3 天）

- [ ] 控制 API（REST + SSE `/events`）+ 随机 token 鉴权
- [ ] Dashboard：账号卡片矩阵（三进度条 + 余额 + 色阶 + 告警徽标）
- [ ] Requests：实时流水表格（模型 / 账号 / token / 缓存命中 / 成本 / TTFT）
- [ ] Accounts / Rules / Settings / Logs 页面

**验收**：断网、401、超额三种状态下 UI 均正确且不白屏；20 连发请求逐条实时出现（<300ms）。

### Phase 4 — 桌面化（2 天）

- [ ] 托盘（含 Linux 无托盘降级路径）、单实例、开机自启
- [ ] stronghold 密钥加密；first-run 引导
- [ ] 日志 ring buffer + 导出；崩溃自动恢复

**验收**：关窗后代理仍可用；强杀 App 后进程树清空；单实例不重复起服务。

### Phase 5 — 打包发布（2 天）

- [ ] CI 矩阵：`macos-14` / `macos-13` / `windows-latest` / `ubuntu-22.04`
- [ ] macOS 签名 + notarytool 公证；Windows signtool（可选）；Linux 免签
- [ ] 三平台 updater 清单合并

**验收**：干净机器上安装即用，首启有引导。

**总工期：约 17–22 个专注日。**

---

## 8. 测试策略

### 8.1 上游错误矩阵（Phase 1 必须全绿）

上游把多种业务拒绝折叠进少数状态码，必须逐格验证：

| 上游响应 | 池中还有可用账号 | 池已耗尽 | 期望行为 |
|---|---|---|---|
| 401 | 换号重试 | 全部标记 disabled | 抛 INVALID_CREDENTIAL，提示检查密钥 |
| 429 | 换号重试 | 探测复活失败 | 抛 RATE_LIMIT，带最早 reset 时间 |
| 402 | **视为额度耗尽**（换号） | 同 429 | 不得当作"上游整体限流" |
| 403 `upgrade_required` | 固定降级到 `/alpha/generate` | — | 记住该 key 的协议偏好（TTL 15min） |
| 403 其他（模型不在套餐） | **不换号**，直接报错 | — | 换号无用，避免误伤整个池 |
| 200 且流中断 | — | — | 不回放、不换号，原样抛 TRANSPORT |

> ⚠️ 最大陷阱：`proxy.mjs` 的 `mapCcError` 把 **402 也映射成 429**。轮换逻辑必须区分"该账号额度耗尽"（换号有用）与"上游整体限流"（换号无用，应退避）。这是合并两套逻辑时最容易写错的地方。

### 8.2 双实现 conformance 测试

`third_party/proxy.mjs` 作为 **oracle** 保留在仓库中（不参与构建）：

1. 同一份输入，分别打到 Node 版代理与 Rust 版代理；
2. 用 mock 上游记录两者发出的**请求头、请求体、事件序列**；
3. 逐字段 diff，任何偏差即失败。

逆向协议的回归靠人眼测不出来；2956 行 JS 最大的价值是**能当参照物**。上游发版后跑一次 conformance，就能知道要不要跟。

### 8.3 其它

- Rust 单测：转换层（消息、工具调用、图片）、SSE 状态机、配额解析
- 前端：Vitest + Testing Library（关键交互）
- 端到端：mock 上游 + Playwright（可选）

---

## 9. 协议备忘（PROTOCOL.md 摘要）

移植时**必须原样保留**的已知坑（均来自上游真机验证）：

| # | 约束 | 后果 |
|---|---|---|
| 1 | `params.system` 必须恒为**字符串**，数组型 content 要展开拼接 | 传数组直接被拒：`expected string, received array` |
| 2 | assistant 回合的 `reasoning` **必须回传且排在 [reasoning, text, tool-call] 最前** | thinking 模式下缺 reasoning 被上游拒绝 |
| 3 | 无 system prompt 时发**空格占位** | 否则上游注入约 7.5K token 默认提示词 |
| 4 | 上游读空闲超时只计 `read()` 等待，每 chunk 重置 | 官方 CLI 无 idle 超时，贸然加超时会误杀健康请求 |
| 5 | 客户端不读时不得让上游流无界堆积 | RSS 随上游流增长（须持续 drain 或 abort） |
| 6 | 413 后转排空模式继续读完丢弃 | 保持 keep-alive 可复用，而非 `Connection reset` |
| 7 | Anthropic 侧 `cache_read` 为**独立增量**，input 只计非缓存部分 | 相加会得到约两倍（OpenAI 侧是相反约定） |
| 8 | thinking block 的 `signature` 需按文本派生伪造 | Anthropic 校验签名，缺失/相同会出问题 |
| 9 | 必须同时监听 `close`/`error` | 否则客户端断连会让请求协程永久挂起 |
| 10 | 下游错误码映射保留 `retry_after` | SDK 才能拿到正确退避提示 |

上游请求头（`/alpha/generate`）：`x-cli-environment: production`、`x-command-code-version`（从 npm registry 动态刷新，24h）、`x-session-id`、`x-co-flag`、`x-taste-learning`、`x-project-slug`（伪）、`traceparent`、可选 `x-cmd-zdr`。

---

## 10. 数据模型

```sql
accounts(
  id INTEGER PRIMARY KEY, label TEXT, key_cipher BLOB, key_hint TEXT, enabled INTEGER,
  created_at INTEGER,
  plan_id TEXT, sub_status TEXT, period_end INTEGER,
  credits_monthly REAL, credits_purchased REAL, credits_free REAL,
  win5_used REAL, win5_cap REAL, win5_exceeded INTEGER, win5_reset_at INTEGER,
  win_week_used REAL, win_week_cap REAL, win_week_exceeded INTEGER, win_week_reset_at INTEGER,
  last_error TEXT, last_checked INTEGER
);
requests(
  id INTEGER PRIMARY KEY, ts INTEGER, account_id INTEGER, model TEXT,
  upstream_protocol TEXT, stream INTEGER, http_status INTEGER, error_code TEXT,
  input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER,
  ttft_ms INTEGER, total_ms INTEGER, retries INTEGER
);
route_rules(id INTEGER PRIMARY KEY, position INTEGER, models_json TEXT, account_id INTEGER);
settings(key TEXT PRIMARY KEY, value TEXT);
```

---

## 11. 控制 API 契约

```
GET    /api/health
GET    /api/accounts                    POST   /api/accounts {label,key}
PATCH  /api/accounts/:id                DELETE /api/accounts/:id
POST   /api/accounts/:id/refresh        POST   /api/accounts/refresh-all
GET    /api/usage/series?range=24h|7d|30d&group=model|account
GET    /api/requests?limit=&cursor=
GET    /api/rules                       PUT    /api/rules
GET    /api/settings                    PATCH  /api/settings
GET    /events                          # SSE: request | quota | account | log
```

全部路由要求 `x-control-token`（启动时随机生成，仅注入 WebView）。

---

## 12. 跨平台注意事项

| 平台 | 事项 |
|---|---|
| **Windows** | `#![windows_subsystem = "windows"]`；首次运行需 WebView2（Win11 自带）；签名后 SmartScreen 才不拦；路径用 `std::path`，禁止硬编码 `/` |
| **macOS** | 分发需 Developer ID 签名 + 公证；arm64/x64 两条产物；不做 universal（无 sidecar 后成本降低，但收益有限） |
| **Linux** | 主推 AppImage，另出 deb/rpm；**固定 `ubuntu-22.04` 构建**保证 glibc 兼容；托盘需 `libayatana-appindicator`，GNOME/Wayland 下需有**无托盘降级路径**（窗口常驻 + 快捷键） |
| 全部 | SQLite 用 `rusqlite` 的 `bundled`；密钥用 stronghold 避免依赖 OS 服务 |

---

## 13. 风险登记

| 风险 | 影响 | 对策 |
|---|---|---|
| 上游未公开 API 漂移 | 配额/生成失效 | 解析层字段容错 + 缺字段降级；探针失败不改变池状态；conformance 测试 |
| 402/429 语义混淆 | 误标整个账号池 | 第 8.1 节矩阵单测强制覆盖 |
| SSE 手写状态机出错 | 流式输出错乱/挂起 | 以 `third_party/proxy.mjs` 为 oracle 做逐事件 diff |
| 上游同步滞后 | 拿不到上游修复 | 记录 commit hash；每期跑 conformance；保留 vendored JS |
| 逆向协议的法律/条款风险 | 账号被封 | 仅使用自己的账号与官方 API；不绕过计费；README 声明非官方 |
| 磁盘 / 构建环境 | 本地构建失败 | 本地仅跑 macOS 目标，其余交给 CI |

---

## 14. 下一步

Phase 0 剩余项：Tauri 模板、axum `/health`、mock 上游、CI 骨架。
随后进入 Phase 1（账号池 + 聊天链路），这是全项目风险最集中的一段。

---

## 附：上游溯源

| 项目 | Commit | 许可 |
|---|---|---|
| MAXeaglet/commandcode-proxy | `a94181240e96f71fc86019ae87e91fa0f3f0478e` | MIT |
| MAXeaglet/commandcode-usage | `03aa55bcfb19258b127db10b818bb091c2120fc8` | MIT |
| Mars-Sea/dsh-commandcode-provider（账号池逻辑） | `7cf3235c9a774f182a5183046f3cd154901b6527` | MIT |
