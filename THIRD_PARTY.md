# Third-party notices / 第三方归属

本项目是**协议翻译层**，其协议知识（Command Code 上游端点、请求形状、事件流、伪装头、配额字段）
来自以下社区项目。它们是 MIT 许可，本项目在其基础上以 Rust 重写并扩展。

保留 `third_party/` 下 vendored 的 JS 源码**仅用于对照测试**（conformance oracle），不参与构建产物。

---

## 1. MAXeaglet/commandcode-proxy

- 仓库：https://github.com/MAXeaglet/commandcode-proxy
- 许可：MIT
- 固化 commit：`a94181240e96f71fc86019ae87e91fa0f3f0478e`
- 用途：OpenAI / Anthropic ↔ Command Code 的协议转换、SSE 翻译、device-fingerprint、生命周期预请求、错误码映射
- 本地副本：`third_party/proxy.mjs`

Copyright (c) MAXeaglet — MIT License

## 2. MAXeaglet/commandcode-usage

- 仓库：https://github.com/MAXeaglet/commandcode-usage
- 许可：MIT
- 固化 commit：`03aa55bcfb19258b127db10b818bb091c2120fc8`
- 用途：多账号配额端点（`/alpha/whoami`、`/alpha/billing/credits`、`/alpha/billing/subscriptions`、`/alpha/usage/summary`）的解析与派生逻辑、月度额度估算、告警判定、面板交互
- 本地副本：`third_party/usage-worker.js`、`third_party/usage-index.html`

Copyright (c) MAXeaglet — MIT License

## 3. Mars-Sea/dsh-commandcode-provider

- 仓库：https://github.com/Mars-Sea/dsh-commandcode-provider
- 许可：MIT
- 固化 commit：`7cf3235c9a774f182a5183046f3cd154901b6527`
- 用途：多账号池的轮换状态机设计（`src/accounts.ts`）、五小时窗口探测与复活策略、能力/套餐快照的数据结构
- 本地副本：`third_party/dsh-accounts.ts`

Copyright (c) Mars-Sea — MIT License

---

## MIT License 全文

以下为上述项目适用的 MIT 许可条款：

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## 免责声明

本项目与 Command Code, Inc. 无任何关联，未获其授权或认可。
它使用你自己的账号、你自有的 API 密钥，并遵守 Command Code 的服务条款与计费规则。
使用者需自行确保其用法符合当地法律与上游服务条款。
