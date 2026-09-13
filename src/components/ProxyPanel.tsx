import { useState } from "react";

import type { Health } from "../lib/api";

/** 代理面板：显示本地端点、服务状态及不同协议客户端的配置指引与复制工具。 */
export function ProxyPanel({ health }: { health: Health | null }): React.JSX.Element {
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  const copyToClipboard = async (text: string, key: string): Promise<void> => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedKey(key);
      window.setTimeout(() => setCopiedKey(null), 2000);
    } catch {
      // 剪贴板不可用时静默
    }
  };

  return (
    <section className="panel proxy-panel">
      <div className="proxy-panel__header">
        <div className="proxy-panel__title-row">
          <h2 className="panel__title" style={{ margin: 0 }}>
            本地代理接入
          </h2>
          {health !== null && (
            <span className="badge badge--ok">已就绪 · {health.accounts} 个账号可用</span>
          )}
        </div>
        {health && (
          <span className="proxy-panel__upstream">
            上游基址: <code>{health.api_base}</code>
          </span>
        )}
      </div>

      <div className="proxy-endpoints">
        {/* OpenAI 兼容协议 */}
        <div className="proxy-endpoint-card">
          <div className="proxy-endpoint-card__top">
            <span className="proxy-protocol-tag tag--openai">OpenAI 兼容协议</span>
            <span className="proxy-endpoint-note">适用于 Cursor, Cline, NextChat 等</span>
          </div>
          <p className="proxy-endpoint-desc">
            端点: <code>/v1/chat/completions</code> 与 <code>/v1/responses</code>
          </p>
          <div className="proxy-endpoint-box">
            <span className="proxy-endpoint-label">Base URL</span>
            <code>http://127.0.0.1:3050/v1</code>
            <button
              type="button"
              className="btn btn--sm"
              onClick={() => void copyToClipboard("http://127.0.0.1:3050/v1", "openai")}
            >
              {copiedKey === "openai" ? "已复制 ✓" : "复制"}
            </button>
          </div>
        </div>

        {/* Anthropic 兼容协议 */}
        <div className="proxy-endpoint-card">
          <div className="proxy-endpoint-card__top">
            <span className="proxy-protocol-tag tag--anthropic">Anthropic 兼容协议</span>
            <span className="proxy-endpoint-note">适用于 Claude Code CLI, Anthropic SDK 等</span>
          </div>
          <p className="proxy-endpoint-desc">
            端点: <code>/v1/messages</code>（官方 SDK 会自动在 Base URL 后追加 <code>/v1/messages</code>）
          </p>
          <div className="proxy-endpoint-box">
            <span className="proxy-endpoint-label">Base URL</span>
            <code>http://127.0.0.1:3050</code>
            <button
              type="button"
              className="btn btn--sm"
              onClick={() => void copyToClipboard("http://127.0.0.1:3050", "anthropic")}
            >
              {copiedKey === "anthropic" ? "已复制 ✓" : "复制"}
            </button>
          </div>
        </div>
      </div>

      <div className="proxy-auth-tip">
        <span>
          💡 <strong>鉴权与 API Key 说明</strong>：实际凭据由 Command Code 桌面端统一安全存储并自动轮换多账号额度。在客户端设置中，API Key 填任意占位值（例如 <code>cc-placeholder</code> 或 <code>sk-dummy</code>）即可正常使用。
        </span>
      </div>
    </section>
  );
}
