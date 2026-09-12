import type { Health } from "../lib/api";

/** 代理面板：显示本地端点与连接状态，并给出客户端接入方式。 */
export function ProxyPanel({ health }: { health: Health | null }): React.JSX.Element {
  return (
    <section className="panel">
      <h2 className="panel__title">本地代理</h2>
      {health === null ? (
        <p className="panel__hint">尚未连接。</p>
      ) : (
        <dl className="kv">
          <dt>状态</dt>
          <dd>{health.status}</dd>
          <dt>已配置账号</dt>
          <dd>{health.accounts}</dd>
          <dt>上游</dt>
          <dd>{health.api_base}</dd>
        </dl>
      )}
      <p className="panel__hint">
        把任意 OpenAI / Anthropic 客户端的 base URL 指到
        <code> http://127.0.0.1:3050/v1 </code>
        即可使用；API key 由本应用统一管理，客户端可填任意占位值。
      </p>
    </section>
  );
}
