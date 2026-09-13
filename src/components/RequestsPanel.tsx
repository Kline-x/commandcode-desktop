import { useCallback, useEffect, useState } from "react";

import { sendJson } from "../lib/api";

/** 一条请求流水在控制面上的形状。 */
interface RequestView {
  id: number;
  at_ms: number;
  account_id: string;
  model: string;
  protocol: string;
  status: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  ttft_ms: number | null;
  total_ms: number;
}

/** 把 epoch 毫秒格式化成 HH:MM:SS（本地时区）。 */
function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

/** 请求流水面板：逐请求展示模型、账号、token 与耗时。 */
export function RequestsPanel(): React.JSX.Element {
  const [rows, setRows] = useState<RequestView[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async (): Promise<void> => {
    try {
      const data = await sendJson<{ requests: RequestView[] }>("POST", "/api/requests/recent", {
        limit: 50,
      });
      setRows(data.requests);
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
                <th>通道</th>
                <th className="num">状态</th>
                <th className="num">输入</th>
                <th className="num">输出</th>
                <th className="num">缓存</th>
                <th className="num">TTFT</th>
                <th className="num">总耗时</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id}>
                  <td>{formatTime(row.at_ms)}</td>
                  <td>
                    <code>{row.model}</code>
                  </td>
                  <td>{row.account_id}</td>
                  <td>
                    <span className="badge">{row.protocol}</span>
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
                  <td className="num code-font">{row.input_tokens.toLocaleString()}</td>
                  <td className="num code-font">{row.output_tokens.toLocaleString()}</td>
                  <td className="num code-font">{row.cached_tokens.toLocaleString()}</td>
                  <td className="num code-font">
                    {row.ttft_ms != null ? `${row.ttft_ms}ms` : "—"}
                  </td>
                  <td className="num code-font">{row.total_ms}ms</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
