# STYLE.md — 代码风格与工程规范

> 本文是**代码层面的规范**；协作流程（分支、PR、合并）见 [CONTRIBUTING.md](../CONTRIBUTING.md)。
> 规范的价值在于**可自动执行**：能用工具强制的就不靠人记。本仓库的全部检查都收敛在
> `scripts/check.sh` 一个入口，CI 与本地钩子调用同一套。

---

## 1. 自动化边界（先看这个）

| 规范 | 执行者 | 是否强制 |
|---|---|---|
| 格式（缩进、换行、空行） | `rustfmt` + `.editorconfig` | ✅ CI 强制 |
| Lint（可疑写法、复杂度） | `clippy` + `clippy.toml` + `[workspace.lints]` | ✅ CI 强制（`-D warnings`） |
| 拼写 | `typos` | ✅ CI 强制 |
| 依赖许可与安全公告 | `cargo-deny` + `deny.toml` | ✅ CI 强制 |
| 不改动 `third_party/` | pre-commit 钩子 | ✅ 本地拦截 |
| 不提交明文密钥 | pre-commit 钩子 | ✅ 本地拦截 |
| 命名、注释、错误信息、测试写法 | **本文档 + code review** | ⚠️ 人工 |

**一次执行全部检查：**

```sh
scripts/check.sh
```

**安装本地钩子（每个 clone 执行一次）：**

```sh
scripts/install-hooks.sh
```

---

## 2. Rust 代码风格

### 2.1 格式

- 一律 `cargo fmt --all`，**不要手工调整换行**。
- 行宽 100。如果你觉得某行挤，通常是**该抽变量或抽函数**了，而不是该放宽行宽。
- 文件编码 UTF-8、换行 LF（`.editorconfig` 已保证；Windows 上也必须是 LF）。

### 2.2 命名

| 对象 | 约定 | 例 |
|---|---|---|
| 类型 / trait / 枚举 | `UpperCamelCase` | `AccountState`、`RejectionKind` |
| 函数 / 变量 / 模块 / 字段 | `snake_case` | `resolve_key`、`now_ms` |
| 常量 / 静态 | `SCREAMING_SNAKE_CASE` | `MAX_ACCOUNT_ROTATIONS` |
| 枚举变体 | `UpperCamelCase` | `AccountStateKind::Cooldown` |

**禁止缩写**（除非是领域通用词）：

- ✅ `reasoning_effort`、`account`、`timestamp_ms`
- ❌ `re`、`acct`、`ts`（`now_ms` 这类**单位后缀**是有价值的，保留）

### 2.3 单位与时间

- **带单位的量必须写进名字**：`timeout_ms`、`reset_at_ms`、`ttft_ms`、`mem_gib`。
  跨协议换算（秒 / 毫秒 / RFC3339）是这类项目最常见的 bug 来源。
- **不在函数内部读系统时间**。时间经参数注入（`now_ms: i64`），这样过期逻辑可以被
  确定性测试覆盖——这条已在 `pool.rs` 落地，是**全仓库的硬性约定**。
- 时间戳统一用 `i64` 毫秒（Unix epoch）。

### 2.4 错误处理

- 库代码（`crates/*`）用 `thiserror` 定义**具体错误枚举**，不用 `anyhow`。
- **禁止 `unwrap()` / `expect()` / `panic!`** 处理运行时输入；只在
  - 测试代码、
  - 编译期可证明的不变量（此时 `expect` 必须写明**为什么不可能失败**）

  两种情况下允许。
- 上游返回的原始错误文本：**截断保存**（≤500 字符），不整段传播。

### 2.5 注释

- 注释解释**为什么**，不解释**是什么**。`// 递增 i` 这种是噪音。
- 公开项写 `///` 文档注释；模块头部写 `//!`。
- **协议相关的代码必须引用 `docs/PROTOCOL.md` 的条目编号**，例如：

  ```rust
  // PROTOCOL.md #1：上游要求 params.system 恒为字符串，数组会被拒。
  // 不要「优化」成块数组。
  ```

  这让后来者能顺着编号找到依据，而不是靠考古。
- 注释和文档用**中文**（本项目主要读者是中文母语者）；标识符、日志键名、协议字段一律**英文**。

---

## 3. 模块与架构约束

分层是**单向依赖**，反向依赖一律拒绝：

```
src-tauri/          外壳：窗口、托盘、密钥、进程编排
      ↓ 只能向下依赖
crates/cc-server/   核心：协议翻译、账号池、配额、存储
      ↓
（第三方 crate）
```

- **`cc-server` 不得依赖 Tauri**。它必须能在 Linux CI 上独立 `cargo test`。
- **纯逻辑与 I/O 分离**：状态机、解析、转换是纯函数（可单测）；网络、磁盘、时间在边界注入。
  `pool.rs` 是范例——零依赖、零 I/O、15 个单测。
- 新增模块时，在 `lib.rs` 中加一行模块声明 + 文档注释说明职责。

---

## 4. 测试规范

- **每个模块自带 `#[cfg(test)] mod tests`**，就近测试。
- 测试名用**句子式描述行为**，不用 `test_xxx`：
  - ✅ `exhausted_preferred_falls_back_to_rotation_order`
  - ❌ `test_resolve_2`
- **断言必须带失败信息**（中文），说明「期望什么、为什么」：

  ```rust
  assert_eq!(picked.slot.id, "default", "首选账号被限流时应回落到轮转顺序");
  ```

- 边界必须显式覆盖：**恰好等于阈值**、**阈值前一刻**、**空输入**、**全不可用**。
  参考 `pool.rs` 的 `cooldown_expires_at_its_reset_time`。
- 协议行为要有 **conformance 测试**（见 `docs/PLAN.md` 第 8.2 节）：改动协议层必须考虑它。

---

## 5. 日志与可观测性

- 日志用结构化字段，**不打**拼接好的长字符串。
- **API key 一律脱敏**为 `user_…xxxx`（前 4 后 4）；这条是红线，pre-commit 也会拦。
- 错误信息遵循「**做什么 + 为什么 + 怎么办**」三段式，中英双语面向用户，
  例如 `pool` 全灭时的提示会带上「最早重置时间」。
- **本地诊断日志与面向用户的错误分离**：前者可详细，后者要克制。

---

## 6. 依赖引入

新增依赖前先自问：

1. 标准库或已引入的 crate 能否解决？
2. 它的许可在 `deny.toml` 白名单内吗？
3. 它会把二进制变大/变慢吗（本项目目标是单二进制 ~15MB）？

流程：

- 依赖**只在根 `Cargo.toml` 的 `[workspace.dependencies]` 声明**，各 crate 用
  `dep.workspace = true` 引用，避免版本漂移。
- 禁止在 `[dependencies]` 里用 `*` 通配版本（`deny.toml` 会拒绝）。
- 引入后跑一次 `cargo deny check`。

---

## 7. 前端（TypeScript / React）

- `strict: true`，禁止 `any`（用 `unknown` + 收窄）。
- 组件文件 `PascalCase.tsx`，工具函数 `camelCase.ts`。
- **业务逻辑放 `src/lib/`**，组件只做渲染——配额派生、告警判定这类逻辑要能被单测。
- 网络访问统一走 `src/lib/api.ts`，不散落 `fetch`。

---

## 8. 提交信息

Conventional Commits，**中文正文**：

```
<type>(<scope>): <一句话摘要>

<为什么这么做；协议相关改动引用 PROTOCOL.md 条目>
```

- `type`：`feat` `fix` `docs` `refactor` `test` `chore` `perf` `ci`
- `scope`：`pool` `sse` `quota` `store` `ui` `ci` `docs` …
- 摘要用**祈使句**，句尾不加句号。
- **一次提交只做一件事**；格式化产生的无关 diff 请单独提交。

---

## 9. 已知豁免（有理由的例外）

| 位置 | 豁免 | 理由 |
|---|---|---|
| `[workspace.lints.clippy]` | `large_enum_variant` | 协议层枚举承载的 JSON 形状差异大，Box 化降低可读性 |
| `[workspace.lints.clippy]` | `doc_markdown` | 中文文档中的标点/术语会被误判 |
| `[workspace.lints.clippy]` | `ptr_arg` | 上游既有签名大量使用 `&String`，改动是破坏性的且无收益 |
| `third_party/` | 全部检查 | 固化副本，不格式化、不检查、不修改 |

新增豁免必须**在此表登记并写明理由**，否则在 review 中会被要求撤掉。
