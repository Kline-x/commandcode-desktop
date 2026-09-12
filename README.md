# commandcode-desktop

**Command Code 桌面客户端** — 多账号轮换代理（OpenAI / Anthropic 兼容）+ 实时额度用量面板，跨平台，单二进制。

> 非官方社区项目，与 Command Code, Inc. 无关联。你需要自己的 Command Code 账号与订阅，并遵守其服务条款。

English | 简体中文（本文）

---

## 它是什么

一个双击即用的桌面应用，为你本机的任意 OpenAI / Anthropic 兼容客户端提供统一的本地端点：

```
你的客户端 ──▶ http://127.0.0.1:3050/v1 ──▶ 账号池（自动轮换）──▶ Command Code
```

- **多账号池**：配置多个 Command Code 账号，某个账号额度耗尽（429/401）时自动切换到下一个，对客户端无感
- **双协议对外**：`/v1/chat/completions`（OpenAI）与 `/v1/messages`（Anthropic），另含 `/v1/responses`、`/v1/models`
- **实时用量**：5 小时滚动 / 每周 / 每月额度进度条、余额、告警徽标
- **逐请求流水**：模型、账号、输入/输出 token、缓存命中、成本、TTFT
- **模型→账号路由**：把特定模型固定路由到特定账号
- **常驻托盘**：关窗不停止代理

## 状态

**早期开发中** — Phase 0 进行中（仓库骨架、核心 crate、CI 已就绪）。完整路线图见 [docs/PLAN.md](docs/PLAN.md)。

## 技术栈

Tauri 2 + 纯 Rust 后端（axum / reqwest / rusqlite / stronghold）+ React + Vite 前端。

选择纯 Rust 而非 Node sidecar 的理由见 [docs/PLAN.md 第 2 节](docs/PLAN.md)。

## 构建（计划中）

```sh
pnpm install
pnpm tauri dev      # 开发
pnpm tauri build    # 打包
```

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| Phase 0 | 骨架、核心 crate、mock 上游、CI 矩阵 | 🚧 进行中 |
| Phase 1 | 账号池 + 轮换 + 聊天链路 | 🚧 状态机已落地，链路待做 |
| Phase 2 | Anthropic 面 + 配额轮询 | ⏳ |
| Phase 3 | 控制 API + 用量面板 | ⏳ |
| Phase 4 | 托盘、密钥加密、日志 | ⏳ |
| Phase 5 | 三平台打包发布 | ⏳ |

## 致谢与许可

协议知识来自两个 MIT 上游项目，详见 [THIRD_PARTY.md](THIRD_PARTY.md)。本项目以 MIT 许可发布。
