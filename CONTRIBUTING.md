# 协作规范

> 目标：**任何人拿到仓库，都能在 10 分钟内知道该怎么写、怎么提交、怎么合并。**
> 代码层面的风格约束见 [docs/STYLE.md](docs/STYLE.md)。

---

## 0. 五分钟上手

```sh
git clone https://github.com/Kline-x/commandcode-desktop.git
cd commandcode-desktop

scripts/install-hooks.sh     # 安装 pre-commit 钩子（每个 clone 一次）
scripts/check.sh             # 跑一遍全部检查，确认环境 OK
```

依赖：Rust stable（由 `rust-toolchain.toml` 固定）+ Node 22 / pnpm 10（Phase 3 起需要前端）。

可选但推荐：

```sh
brew install typos-cli                 # 拼写检查
cargo install cargo-deny --locked      # 依赖许可与安全审查
```

---

## 1. 分支模型

从最新的 `main` 切分支，命名格式 `<type>/<简短描述>`：

| 前缀 | 用途 | 例 |
|---|---|---|
| `feat/` | 新功能 | `feat/pool-rotation` |
| `fix/` | 修 bug | `fix/sse-reasoning-close` |
| `docs/` | 文档 | `docs/protocol-notes` |
| `refactor/` | 重构（行为不变） | `refactor/split-convert` |
| `test/` | 补测试 | `test/error-matrix` |
| `chore/` | 构建/依赖/杂项 | `chore/bump-axum` |

```sh
git checkout main && git pull
git checkout -b feat/pool-rotation
```

**不要**直接在 `main` 上改。

---

## 2. 写代码

- 风格与命名：见 [docs/STYLE.md](docs/STYLE.md)
- 协议相关改动：先读 [docs/PROTOCOL.md](docs/PROTOCOL.md)，改完在代码里引用条目编号
- 提交前跑：

```sh
scripts/check.sh
```

pre-commit 钩子会自动跑其中**快速**的那部分（fmt / clippy / test 核心 crate）。
紧急情况可 `git commit --no-verify`，但 CI 仍会拦。

---

## 3. 提交

Conventional Commits，**中文正文**：

```
feat(pool): 支持按模型路由到指定账号

路由规则是提示而非硬门禁：命中的账号不可用时回落到常规轮转，
避免一条规则把请求钉死在一个已耗尽的账号上。

PROTOCOL.md #6：403 仅在 upgrade_required 时降级到 /alpha/generate，
其余 403（模型不在套餐）不得换号，否则会误伤整个账号池。
```

- **一次提交只做一件事**；格式化产生的无关 diff 单独提交
- 摘要用祈使句、句尾不加句号
- 正文写**为什么**，不写「改了哪些文件」（diff 自己会说）

---

## 4. 开 PR

```sh
git push -u origin feat/pool-rotation
gh pr create --base main --fill        # 或用网页
```

PR 标题与提交信息同格式。描述里写清：

1. **做了什么 / 为什么**
2. **怎么验证的**（贴 `scripts/check.sh` 的关键输出）
3. **风险与影响面**（是否触碰协议层、是否影响 conformance 测试）

### 必须满足的条件

- [ ] `scripts/check.sh` 全绿
- [ ] CI `check` 全绿
- [ ] 至少 1 人 review 通过
- [ ] 没有未解决的 review comment
- [ ] 没有把 `third_party/`、密钥、构建产物带进 PR

### Draft PR

没写完但想让人先看架构，用 `gh pr create --draft`。**不要**把未完成的 PR 标成 Ready。

---

## 5. 合并

- **squash merge**（保持 `main` 线性、每个 PR 一个提交）
- 合并信息用 PR 标题，正文保留要点
- 合并后删除远端分支：

  ```sh
  gh pr merge <n> --squash --delete-branch
  ```

> ⚠️ **本仓库无法启用 GitHub 分支保护**（私有仓库 + Free 计划需 Pro 才能用
> branch protection / rulesets）。因此上面这些是**约定而非强制**——请自觉遵守，
> 不要直接推 `main`。详见 `docs/PLAN.md` 第 15 节。

---

## 6. Review 关注点

按重要性排序，reviewer 请重点看：

1. **协议正确性** —— 是否违反 `docs/PROTOCOL.md` 的硬约束（这类 bug 最难发现）
2. **边界与失败路径** —— 超时、断流、全池耗尽、字段缺失时的行为
3. **错误信息** —— 是否包含「做什么 / 为什么 / 怎么办」，是否双语
4. **测试** —— 边界是否显式覆盖；断言是否带说明
5. **风格** —— 命名、单位后缀、注释是否解释「为什么」

自动化能查的（格式、lint、拼写、许可）不要占用 review 时间。

---

## 7. 红线

- ❌ **不修改 `third_party/`**：那是按 commit 固化的上游副本（conformance oracle）。
  需要更新时整文件替换，并同步 `third_party/upstream.json` 里的 commit hash。
- ❌ **不提交任何真实 API key**：日志、测试夹具、文档里一律用 `user_…xxxx` 占位。
- ❌ **不提交密钥、数据库、构建产物**（`.gitignore` 已覆盖，别用 `-f` 绕过）。
- ❌ **不让 `cc-server` 依赖 Tauri**：它在 Linux CI 上必须能独立测试。
- ❌ **不在库代码里 `unwrap()`**（见 STYLE.md 2.4）。

---

## 8. CI 与成本

私有仓库的 Actions 消耗额度，macOS runner 计费是 Linux 的 **10 倍**：

- 日常 PR 只跑 Linux 上的 `cc-server`（fmt / clippy / test / typos / deny）
- 三平台打包**仅在打 tag 时**进行

**请在本机把测试跑完再推**，不要把 CI 当编译器使用。
