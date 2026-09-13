import { useCallback, useEffect, useMemo, useState } from "react";

import { sendJson } from "../lib/api";

/** 一条请求流水在控制面上的形状。 */
interface RequestView {
  id: number;
  at_ms: number;
  account_id: string;
  account_label?: string | null;
  model: string;
  protocol: string; // 上游通道: cli / openai
  client_protocol?: string; // 客户端协议: openai_chat / anthropic / openai_responses
  status: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  ttft_ms: number | null;
  total_ms: number;
  cost_usd?: number;
}

interface AccountItem {
  id: number;
  label: string;
  key_hint: string;
}

/** 把 epoch 毫秒格式化成 HH:MM:SS（本地时区）。 */
function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

/** 预估并格式化消耗金额（美元）。 */
function estimateCost(row: RequestView): number {
  if (row.cost_usd != null && row.cost_usd > 0) {
    return row.cost_usd;
  }
  if (row.status >= 400 || (row.input_tokens === 0 && row.output_tokens === 0)) {
    return 0;
  }
  const m = row.model.toLowerCase();
  let pIn = 1.0;
  let pCache = 0.25;
  let pOut = 3.0;
  if (
    m.includes("claude-3-7-sonnet") ||
    m.includes("claude-3.7-sonnet") ||
    m.includes("claude-3-5-sonnet") ||
    m.includes("claude-3.5-sonnet")
  ) {
    pIn = 3.0;
    pCache = 0.3;
    pOut = 15.0;
  } else if (m.includes("claude-3-5-haiku") || m.includes("claude-3-haiku")) {
    pIn = 0.8;
    pCache = 0.08;
    pOut = 4.0;
  } else if (m.includes("claude-3-opus")) {
    pIn = 15.0;
    pCache = 1.5;
    pOut = 75.0;
  } else if (m.includes("gpt-4o-mini")) {
    pIn = 0.15;
    pCache = 0.075;
    pOut = 0.6;
  } else if (m.includes("gpt-4o")) {
    pIn = 2.5;
    pCache = 1.25;
    pOut = 10.0;
  } else if (m.includes("o1-mini") || m.includes("o3-mini")) {
    pIn = 1.1;
    pCache = 0.55;
    pOut = 4.4;
  } else if (m.includes("o1")) {
    pIn = 15.0;
    pCache = 7.5;
    pOut = 60.0;
  } else if (
    m.includes("deepseek-v4-pro") ||
    m.includes("deepseek-reasoner") ||
    m.includes("deepseek-r1")
  ) {
    pIn = 0.55;
    pCache = 0.14;
    pOut = 2.19;
  } else if (
    m.includes("deepseek-v4-flash") ||
    m.includes("deepseek-chat") ||
    m.includes("deepseek-v3")
  ) {
    pIn = 0.14;
    pCache = 0.014;
    pOut = 0.28;
  } else if (m.includes("gemini-2.0-flash") || m.includes("gemini-1.5-flash")) {
    pIn = 0.1;
    pCache = 0.025;
    pOut = 0.4;
  } else if (m.includes("gemini-2.0-pro") || m.includes("gemini-1.5-pro")) {
    pIn = 1.25;
    pCache = 0.3125;
    pOut = 5.0;
  }
  const uncachedIn = Math.max(0, row.input_tokens - row.cached_tokens);
  return (uncachedIn * pIn + row.cached_tokens * pCache + row.output_tokens * pOut) / 1_000_000;
}

function formatCost(cost: number): string {
  if (cost <= 0) return "$0.0000";
  if (cost < 0.0001) return "<$0.0001";
  return `$${cost.toFixed(4)}`;
}

/** 渲染客户端协议徽标。 */
function renderClientProtocol(proto?: string): React.JSX.Element {
  switch (proto) {
    case "anthropic":
      return <span className="badge badge--proto-anthropic">Anthropic</span>;
    case "openai_responses":
      return <span className="badge badge--proto-responses">Responses</span>;
    case "openai_chat":
    default:
      return <span className="badge badge--proto-chat">OpenAI Chat</span>;
  }
}

/** 请求流水面板：逐请求展示模型、账号展示名、客户端协议、金额、token 与耗时。 */
export function RequestsPanel(): React.JSX.Element {
  const [rows, setRows] = useState<RequestView[] | null>(null);
  const [accounts, setAccounts] = useState<AccountItem[]>([]);
  const [error, setError] = useState<string | null>(null);

  const accountMap = useMemo(() => {
    const map = new Map<string, { label: string; key_hint: string }>();
    for (const acc of accounts) {
      map.set(String(acc.id), { label: acc.label, key_hint: acc.key_hint });
    }
    return map;
  }, [accounts]);

  const load = useCallback(async (): Promise<void> => {
    try {
      const [reqData, accData] = await Promise.all([
        sendJson<{ requests: RequestView[] }>("POST", "/api/requests/recent", {
          limit: 50,
        }),
        sendJson<{ accounts: AccountItem[] }>("POST", "/api/accounts/list", {}).catch(() => ({
          accounts: [],
        })),
      ]);
      setRows(reqData.requests);
      if (accData.accounts) {
        setAccounts(accData.accounts);
      }
      setError(null);
    } catch (cause) {
      setRows(null);
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  useEffect(() => {
    void load();
    // 流水是「准实时」：控制面每 2 秒拉一次足够，不必做逐条推送
    const timer = window.setInterval(() => void load(), 2_000);
    return () => window.clearInterval(timer);
  }, [load]);

  const resolveAccountName = (row: RequestView): { name: string; title: string } => {
    const acc = accountMap.get(row.account_id);
    if (row.account_label && row.account_label.trim() !== "") {
      return {
        name: row.account_label,
        title: acc?.key_hint ? `${acc.key_hint} (ID: ${row.account_id})` : `ID: ${row.account_id}`,
      };
    }
    if (acc) {
      const label = acc.label?.trim();
      return {
        name: label ? label : acc.key_hint || `账号 #${row.account_id}`,
        title: `ID: ${row.account_id}${acc.key_hint ? ` · ${acc.key_hint}` : ""}`,
      };
    }
    return {
      name: row.account_id === "(无账号)" ? "(无可用账号)" : `账号 #${row.account_id}`,
      title: `ID: ${row.account_id}`,
    };
  };

  return (
    <section className="panel">
      <h2 className="panel__title">最近请求</h2>
      {error !== null && <p className="panel__hint">读取失败：{error}</p>}
      {rows === null ? (
        <p className="panel__hint">加载中…</p>
      ) : rows.length === 0 ? (
        <p className="panel__hint">还没有请求记录。</p>
      ) : (
        <div className="table-wrapper">
          <table>
            <thead>
              <tr>
                <th>时间</th>
                <th>模型</th>
                <th>账号</th>
                <th>客户端协议</th>
                <th>通道</th>
                <th className="num">状态</th>
                <th className="num">消耗金额</th>
                <th className="num">输入</th>
                <th className="num">输出</th>
                <th className="num">缓存</th>
                <th className="num">TTFT</th>
                <th className="num">总耗时</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const accInfo = resolveAccountName(row);
                const cost = estimateCost(row);
                return (
                  <tr key={row.id}>
                    <td>{formatTime(row.at_ms)}</td>
                    <td>
                      <code>{row.model}</code>
                    </td>
                    <td>
                      <span className="acc-name-cell" title={accInfo.title}>
                        {accInfo.name}
                      </span>
                    </td>
                    <td>{renderClientProtocol(row.client_protocol)}</td>
                    <td>
                      <span className="badge badge--channel">{row.protocol.toUpperCase()}</span>
                    </td>
                    <td className="num">
                      <span
                        className={`badge ${
                          row.status >= 200 && row.status < 300
                            ? "badge--ok"
                            : row.status >= 400 && row.status < 500
                            ? "badge--warn"
                            : "badge--err"
                        }`}
                      >
                        {row.status}
                      </span>
                    </td>
                    <td className="num code-font cost-cell">{formatCost(cost)}</td>
                    <td className="num code-font">{row.input_tokens.toLocaleString()}</td>
                    <td className="num code-font">{row.output_tokens.toLocaleString()}</td>
                    <td className="num code-font">{row.cached_tokens.toLocaleString()}</td>
                    <td className="num code-font">
                      {row.ttft_ms != null ? `${row.ttft_ms}ms` : "—"}
                    </td>
                    <td className="num code-font">{row.total_ms}ms</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
