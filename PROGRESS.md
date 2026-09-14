# PROGRESS — commandcode-desktop

> 阶段式进度记录。每次重要阶段边界追加一条，不要覆盖历史。

## 2026-09-13 · 项目认知（discovery）

**当前目标**：梳理仓库现状，确认可运行/可测试状态，为后续开发选定切入点。

**已完成**
- 通读 README、docs/PLAN.md、docs/ARCHITECTURE.md、docs/PROTOCOL.md、CONTRIBUTING.md、THIRD_PARTY.md。
- 摸清代码结构：Rust 工作区（`crates/cc-server` 平台无关核心 + `src-tauri` 桌面外壳）+ React/Vite 前端（`src/`）。
- 确认 git 状态：当前分支 `feat/phase1-protocol`，与 `origin/feat/phase1-protocol` 同步，领先 `main` 21 个提交，工作区干净。

**验证执行**
- `cargo test -p cc-server`：260 + 18 + 11 单测/集成测试全绿（1 个 doc-test ignored）。
- `cargo check -p commandcode-desktop`：通过（Tauri 外壳可编译）。
- `pnpm exec tsc --noEmit`：前端类型检查通过。
- 未跑 `cargo clippy` / `cargo deny` / `typos` / `vite build`（本次仅做认知，未改代码）。

**已知问题**
- 文档状态与代码漂移：README.md / docs/PLAN.md 顶部仍写“Phase 0 进行中”，而实际代码已覆盖 Phase 0–5（见 PLAN 第 4.5 节“实施状态”，产物 18 MB .app + 5.9 MB .dmg 本机实测可运行）。文档状态待同步。
- PLAN 第 6 节的目录契约（`src-tauri/src/proxy/...` 分目录）与实际平铺结构不一致，属已记录的偏差，不影响构建。

**下一步候选**
1. 校准 README/PLAN 的阶段状态，让文档与实现一致。
2. 真机启动应用做一次端到端验收（PLAN 强调的“实际启动并观察”）。
3. 按活跃分支方向继续 Phase 1 协议相关工作（当前分支名即 phase1-protocol）。

## 2026-09-13 · 修复 macOS 托盘图标不显示

**当前目标**：修复 macOS 系统托盘/菜单栏中托盘图标隐形不可见、无法点击的问题。

**根因分析**
- `src-tauri/src/tray.rs` 中在初始化 `TrayIconBuilder` 时未设置 `.icon(...)`，macOS `NSStatusItem` 处于无图且无文本的 0 宽状态。
- `src-tauri/tauri.conf.json` 中缺少 `app.trayIcon` 配置。
- `src-tauri/Cargo.toml` 中 `tauri` 依赖未开启 `image-png` 特性，无法在运行时直接加载内置 PNG 图标。

**实施变更**
- 在 `src-tauri/Cargo.toml` 开启 `tauri` 的 `image-png` 特性。
- 在 `src-tauri/tauri.conf.json` 添加 `app.trayIcon` 配置（指定 `icons/32x32.png`，`iconAsTemplate: false`）。
- 在 `src-tauri/src/tray.rs` 中实现双重保险：优先通过 `app.tray_by_id("main")` 绑定菜单与点击事件；若未预建则动态通过 `TrayIconBuilder` 载入 `32x32.png` 构造托盘；增加左键点击唤起主窗口回调；并在 macOS 下实现“复制本地端点”直接写入 `pbcopy`。

**验证与交付**
- 执行 `./scripts/check.sh`：Rust 260 项测试、Clippy（-D warnings）、cargo-deny、TypeScript 类型检查、Vite 构建全部通过。
- 执行 `pnpm tauri build --bundles app` 生成 Release 应用。
- 更新并部署到 `/Applications/Command Code.app` 并通过 LaunchServices 注册与启动。

## 2026-09-13 · 优化请求流水（消耗金额、客户端协议与账号名称）

**当前目标**：在请求日志面板补充「消耗金额」、「客户端协议」等关键列，并将账号列从单一 ID 编号升级为展示实际账号名称/备注。

**实施变更**
- **协议与核心模型**：
  - `crates/cc-server/src/config.rs`：为 `PublicProtocol` 扩展 `as_str()` 方法。
  - `crates/cc-server/src/proxy.rs`：新增 `estimate_cost_usd` 模型消耗计费函数；`RequestRecord` 新增 `account_label`、`client_protocol` 与 `cost_usd` 字段，并在各生成阶段注入。
  - `crates/cc-server/src/store.rs`：升版 schema 至 v2，安全执行 ALTER TABLE 兼容已有数据库迁移；`requests` 表与 `RequestRow`/`NewRequest` 扩展字段，`REQUEST_SELECT` 联合 `accounts` 表自动回退与派生展示名。
  - `crates/cc-server/src/control.rs`：`RequestView` 结构扩展 `account_label`、`client_protocol` 与 `cost_usd` 并完成序列化映射。
  - `src-tauri/src/bootstrap.rs`：持久化同步写入新增字段至 SQLite。
- **前端 UI 与交互**：
  - `src/components/RequestsPanel.tsx`：新增「客户端协议」（OpenAI Chat / Anthropic / Responses 彩色徽标）与「消耗金额」（精确格式化为美元）列；账号列智能匹配账号备注、密钥提示或编号，悬停展示详情提示。
  - `src/styles.css`：为不同协议徽标、通道徽标、账号名称和金额单元格增加专用样式与高亮。

**验证与交付**
- 执行 `./scripts/check.sh`：260 项核心单测、18 项端到端轮转测试、Clippy 零警告、TypeScript 及 Vite 打包全部通过。
- 执行 `pnpm tauri build --bundles app` 完成生产构建并部署覆盖 `/Applications/Command Code.app` 启动运行。

## 2026-09-13 · 额度单位补充、客户端协议精准识别与独特应用图标重构

**当前目标**：
1. 解决账号用量面板缺少单位提示的问题（已用及额度增加 `$`）；
2. 彻底修复客户端协议识别问题（Responses API 输入容错、多级 Base URL 容错路由与智能协议特征嗅探，确保 OpenAI Chat、Anthropic、Responses 精准分类记录）；
3. 重新设计完全独特的 macOS 应用图标（摆脱同质化的终端 `> _` 风格，融合 Apple Command 键 ⌘ 与量子多环轮转）；
4. 保持项目进度与交付日志实时更新。

**实施变更**
- **额度展示与货币单位**：
  - `src/components/AccountCard.tsx`：引入 `fmtMoney` 格式化函数，用量进度条展示为 `已用 $X / $Y`、`剩余 $Z`；在月度剩余、充值余额、免费额度中统一补充 `$` 单位提示，消除数值歧义。
- **协议兼容与识别引擎**：
  - `crates/cc-server/src/responses.rs`：重构 `convert_responses_to_chat` 针对 `input` 数组项的解析逻辑，兼容缺少 `type: "message"` 但带 `role` 的消息对象及纯字符串项，杜绝 502 `input is required` 报错。
  - `crates/cc-server/src/proxy.rs`：
    - 路由增加 `/v1/v1/responses` 容错路径，适配各 SDK 在 Base URL 后级联追加的场景；
    - 在 `chat_completions` 网关入口增加智能嗅探：若检测到 `anthropic-version` 请求头则智能转发至 Anthropic 管线；若检测到 `input` 且无 `messages` 则自动转换为 Responses 管线并标记正确协议。
  - `crates/cc-server/tests/e2e_rotation.rs`：新增 `client_protocol_is_accurately_recorded_for_all_protocols` 端到端测试，全面覆盖 OpenAI Chat、Anthropic、Responses、容错路由及智能嗅探 6 种场景。
- **独特视觉图标重构**：
  - `scripts/generate_icon.py`：使用超采样纯几何布尔掩模算法（CSG）生成完美正切圆角、零瑕疵接缝的 Apple Command 键（⌘）与外围量子能量光环，中心嵌入高亮能量星核；
  - 重新渲染并输出全部规格：`icon-1024.png`、`icon.png` (512x512)、`128x128@2x.png` (256x256)、`128x128.png`、`32x32.png`，并通过 `iconutil` 生成最新 `icon.icns` 与 `Command Code.icns`。

**验证与交付**
- 执行 Rust 单测与端到端集成测试，6 种协议流转测试全部通过；
- 执行 `./scripts/check.sh`：代码格式、Clippy、Cargo Deny、TypeScript 及 Vite 前端打包全绿；
- 执行 `pnpm tauri build --bundles app` 完成桌面端 Release 打包；
- 部署覆盖至 `/Applications/Command Code.app`，刷新 macOS LaunchServices 图标缓存并重启运行。

---

## [2026-09-13] v0.1.0 正式发版与主分支合并

- **分支合并**：成功将 `feat/phase1-protocol` 全量功能及后续全部优化合并至 `main` 主分支。
- **版本提升**：版本号统一升版至 `v0.1.0`（`Cargo.toml`、`package.json`、`src-tauri/tauri.conf.json`、`Cargo.lock`）。
- **全套质量验证**：`./scripts/check.sh` 全检通过（260 项单元测试 + 19 项 E2E 测试 + 11 项 Mock 上游测试全通过，TypeScript 与 Vite 零报错）。
- **Release 打包**：完成生产环境 Bundle 构建，生成 `Command Code.app` (v0.1.0) 并更新部署到系统 `/Applications/Command Code.app`。
- **发布 Tag**：创建 `v0.1.0` Git Tag 并推送至 GitHub，触发全平台构建流水线。

## [2026-09-13] 代码审计：修复 5 个「单测全绿但功能实际不工作」的缺陷

**当前目标**：对 v0.1.0 做一次系统性代码审计，找出文档声称可用、测试全绿、
但实际机制并未生效的问题。

**审计方法**：不依赖阅读结论，对每个可疑点写**可复现的失败测试**再修。
5 个缺陷全部先用临时用例复现（观察到实际错误状态码），确认后再改代码。

**已修复**

| # | 缺陷 | 根因 | 修复 |
|---|---|---|---|
| 1 | 账号一旦 429 就永久不可用，错误文案却承诺「窗口重置后自动恢复」 | `AccountPool::apply_probe` 无任何生产调用方（只有测试），`quota_poller` 又刻意不持有池 → 标记只进不出 | 新增 `ProxyState::apply_probe_for_slot` + `window_probe_from_snapshot`，bootstrap 的配额订阅循环把快照反馈给池 |
| 2 | 一次 403「模型不在套餐」废掉整个账号 | `mark_rejected` 在判定 `rotates_account()` 之前无条件调用 | 标记移入 `if error.rotates_account()` |
| 3 | 大上下文请求在本地被 413 拒绝（编码代理不可用） | axum `Bytes` 默认体限 2 MB，参考实现是 100 MB | `Config::max_body_bytes`（100 MB）+ `DefaultBodyLimit::max`；mock 上游同步放宽 |
| 4 | 设置页「保留条数」是假设置 | retention 被 bootstrap 硬编码，`Store.retention` 只读无 setter | `AtomicI64` + `set_retention`（立即裁剪），控制面特判，启动时从设置表读取 |
| 5 | 「在访达中打开」点击无反应 | `reveal_data_dir` 只写日志就返回 Ok | 接入 `tauri-plugin-opener`，失败如实回报 |

**顺带修正**：`rotates_account()` 漏了 402（PROTOCOL.md 第 6 节明确要求 402 换号）；
文档漂移（README/PLAN 的 Phase 状态、ARCHITECTURE 的 stronghold/SSE/模块结构表述）。

**验证**
- `cargo test -p cc-server`：**265 单元 + 23 E2E + 11 mock = 299 全绿**
  （新增 4 条端到端回归 + 5 条单测）。
- `cargo clippy -p cc-server --all-targets -- -D warnings`：零警告。
- `cargo fmt --all -- --check`、`pnpm exec tsc --noEmit`：通过。
- `cargo check -p commandcode-desktop`：通过。

**关键教训**：5 个缺陷中 3 个是「模块有完整测试但没接线」，2 个是「判定写对了、
副作用没跟上」。**单测覆盖率 ≠ 链路完整性**——回归测试必须覆盖副作用与恢复路径，
而不是只覆盖正向的成功路径。详见 `docs/PLAN.md` 第 4.6 节。

**未修复（已知缺口，已如实记录进 PLAN）**
- conformance 测试（PLAN 8.2）始终未实现，`third_party/` 目前只是人工参照物。
- 前端无组件测试，关键交互靠手测。
- `Config::listen_addr`、`proxy_token`、`/events` SSE、`/api/usage/series` 未实现，
  文档已改为陈述实际行为而非宣称能力。


