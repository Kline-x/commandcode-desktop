# PROTOCOL.md — Command Code 上游协议备忘

> 本文件记录移植时必须**逐条保留**的约束。全部来自上游项目的真机验证与抓包，
> 不是推测。修改任何一条前请先跑 conformance 测试（见 docs/PLAN.md 第 8.2 节）。

## 1. 端点

| 用途 | 端点 | 备注 |
|---|---|---|
| 生成（主） | `POST {apiBase}/alpha/generate` | 逆向所得，**Go 套餐唯一可用**的通道 |
| 生成（Pro+） | `POST {apiBase}/provider/v1/chat/completions` | 文档化的 OpenAI 兼容面 |
| 生成（Pro+） | `POST {apiBase}/provider/v1/messages` | 文档化的 Anthropic Messages 面 |
| 模型目录 | `GET {apiBase}/provider/v1/models` | 无 key 亦可浏览 |
| 搜索 | `POST {apiBase}/alpha/web-search` | 复用同一 key |
| 配额 | `GET {apiBase}/alpha/{whoami,billing/credits,billing/subscriptions,usage/summary}` | 面板数据源 |
| 指纹/生命周期 | `POST {apiBase}/alpha/fingerprint/record`、`/alpha/lifecycle-events` | 伪装所需 |
| 默认 apiBase | `https://api.commandcode.ai` | |

## 2. `/alpha/generate` 请求形状

```jsonc
{
  "config": { "workingDir", "date", "environment", "structure", "isGitRepo",
              "currentBranch", "mainBranch", "gitStatus", "recentCommits" },
  "memory": null, "taste": null, "skills": null,
  "params": {
    "model": "...",
    "messages": [ /* 见第 3 节 */ ],
    "tools": [ { "type": "function", "name", "description", "input_schema" } ],
    "system": "必须是字符串",
    "max_tokens": 64000,
    "temperature": 0.3,
    "stream": true,
    "reasoning_effort": "low|medium|high|xhigh|max"   // 仅支持的模型
  },
  "threadId": "<uuid>"
}
```

### 请求头

```
Content-Type: application/json
Authorization: Bearer <apiKey>
x-cli-environment: production
x-command-code-version: <从 npm registry 动态刷新，24h 缓存>
x-session-id: <每 key 稳定，有有效期>
x-co-flag: false
x-taste-learning: false
x-project-slug: <伪造>
traceparent: <生成>
x-cmd-zdr: 1                       // 可选，请求 ZDR-only 路由
```

## 3. 消息与内容块

**CLI 形状**（`params.messages`）：

```jsonc
{ "role": "user",      "content": [ {"type":"text","text":"..."},
                                    {"type":"image","image":"data:image/jpeg;base64,..."} ] }
{ "role": "assistant", "content": [ {"type":"reasoning","text":"..."},   // 必须最前
                                    {"type":"text","text":"..."},
                                    {"type":"tool-call","toolCallId","toolName","input":{}} ] }
{ "role": "tool",      "content": [ {"type":"tool-result","toolCallId","toolName",
                                    "output":{"type":"text|error-text","value":"..."}} ] }
```

## 4. 必须保留的硬约束（坑位表）

| # | 约束 | 违反后果 |
|---|---|---|
| 1 | `params.system` 恒为**字符串**；数组 content 要展开取 text 拼接，**不得**输出 Anthropic 风格块数组 | `Validation error: expected string, received array at "params.system"` |
| 2 | assistant 的 `reasoning` **必须回传**，且次序为 `[reasoning, text, tool-call]` | thinking 模式下上游直接拒绝 |
| 3 | 无 system prompt 时发**一个空格**占位 | 否则上游注入约 7.5K token 默认提示词（输入 token 从 ~85 涨到 ~7653） |
| 4 | 上游读空闲超时只计 `read()` 等待，每个 chunk 重置；官方 CLI **不设** idle 上限 | 贸然加超时会误杀长思考的健康请求 |
| 5 | 任何 `tool-call` 必须有配对的 tool result 才回放 | 否则上游报 `Tool result is missing` |
| 6 | 未知 role 归一化为 `user` 且 content 为数组 | 上游校验拒绝 |
| 7 | tool 的 `input_schema` 根必须是 `type: "object"` | 第三方手写 schema / MCP 会导致整轮失败 |
| 8 | 客户端不读时不得让上游流无界堆积 | RSS 随上游流增长（issue #20） |
| 9 | 413 后进入排空模式：继续读完并丢弃 | 否则返回 `Connection reset` 而非 413（issue #7） |
| 10 | 必须同时监听 `close` 与 `error` | 客户端断连会让请求协程永久挂起 |
| 11 | 请求体在内存中会有约 **5.1–7.4×** 的放大 | 100MB 上限意味着单请求最高约 550MB |
| 12 | 不支持 `stop` 序列 | 带 stop 的请求直接失败 |

## 5. 流式事件（`/alpha/generate` → 内部）

```
text-delta | reasoning-start | reasoning-delta | reasoning-end
tool-call | tool-result | finish | error
```

对应到内部流块的组装规则：同一时刻**最多一个 text 块和一个 reasoning 块**打开；
CLI 的 `tool-call` 即时下发，OpenAI 侧的 tool_call 分片需缓冲到 `finish` 再 flush。

## 6. 错误码语义

| 上游 | 含义 | 处理 |
|---|---|---|
| 401 | key 缺失/无效 | 换号；全挂 → `INVALID_CREDENTIAL` |
| 402 | 额度耗尽 → 被上游映射成 429 | **视为该账号额度耗尽**，换号 |
| 403 `upgrade_required` | 该 key 无 Provider API 权限（Go） | 固定降级到 `/alpha/generate`，记忆 15 分钟 |
| 403 其他 | 模型不在套餐 / CLI 版本过低 | **不换号**，直接报错 |
| 429 | 限流/窗口耗尽 | 换号；全挂 → 探测复活 → 带最早 reset 时间 |
| 413 | 请求体过大 | 排空连接，返回 413 |

> ⚠️ 402 与 429 在上游被折叠为同一个状态码。轮换层必须区分"**该账号**额度耗尽"（换号有效）
> 与"上游整体限流"（换号无效，应退避）。

## 7. 配额端点字段

```jsonc
GET /alpha/whoami                 → { user:{id,name,userName}, org?:{id} }
GET /alpha/billing/credits        → { credits:{ monthlyCredits, purchasedCredits, freeCredits, planId? },
                                      windowLimits:{ fiveHour:{used,cap,exceeded,resetAt},
                                                     weekly:{used,cap,exceeded,resetAt} },
                                      limited, exceeded, belowThreshold }
GET /alpha/billing/subscriptions  → { data:{ planId, status, currentPeriodEnd, ... } }
      ?orgId=<whoami.org.id>
GET /alpha/usage/summary          → 计费周期汇总（requests / success rate / tokens / credits）
```

解析要求：字段缺失时降级而非 panic；某个端点失败不影响其余端点；探针失败**不得**改变账号池状态。
月度额度为**派生值**：cap 来自套餐映射，used = cap − remaining，reset = 计费周期结束。
