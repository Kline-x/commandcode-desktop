# 协作约定

> ⚠️ 本仓库为**私有仓库 + GitHub Free 计划**，无法启用分支保护（该功能需要 Pro）。
> 因此 `main` 的保护是**约定**而非强制——请自觉遵守，勿直接推 `main`。

## 流程

1. 从 `main` 切分支：`feat/xxx`、`fix/xxx`、`docs/xxx`、`chore/xxx`
2. 提交后在本地跑通检查（见下）
3. 开 PR 到 `main`，等 CI 变绿
4. 至少 1 人 review 后由**自己**合并（squash 优先，保持历史线性）

## 本地检查（提交前必跑）

```sh
cargo fmt --all -- --check
cargo clippy -p cc-server --all-targets -- -D warnings
cargo test -p cc-server
```

## 提交信息

用 [Conventional Commits](https://www.conventionalcommits.org/)：`type(scope): 摘要`，例如

```
feat(pool): 支持按模型路由到指定账号
fix(sse): 修正 reasoning 块在工具调用后的关闭时机
```

正文说明**为什么**这么做；协议相关的改动请引用 `docs/PROTOCOL.md` 的条目编号。

## 红线

- **不要修改 `third_party/`**：它是按 commit 固化的上游副本（conformance oracle）。需要更新时整文件替换并同步 `third_party/upstream.json`。
- **不要在日志或错误信息里输出完整 API key**：一律脱敏为 `user_…xxxx`。
- **不要把密钥、`*.db`、构建产物提交进仓库**（见 `.gitignore`）。
- 协议行为的改动（请求形状、事件流、错误映射）必须在 PR 描述里说明依据，并考虑对 conformance 测试的影响。

## CI 成本提醒

私有仓库的 Actions 消耗额度，且 macOS runner 计费是 Linux 的 **10 倍**。
日常 PR 只会跑 Linux 上的 `cc-server`；三平台打包仅在打 tag 时进行。
请在本地把测试跑完再推，避免用 CI 当编译器。
