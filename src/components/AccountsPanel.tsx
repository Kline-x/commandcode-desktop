import { useCallback, useEffect, useState } from "react";

import { addAccount, sendJson } from "../lib/api";
import { getAccountAlerts, QuotaCard } from "./QuotaBar";

/** 一个账号在控制面上的形状。 */
interface AccountView {
  id: number;
  label: string;
  key_hint: string;
  enabled: boolean;
  quota: unknown;
  last_error: string | null;
  last_checked_ms: number | null;
}

/**
 * 账号面板：清单 + 添加表单。
 *
 * 添加走 Tauri IPC（`addAccount`）而不是控制面 HTTP——明文密钥的加密必须
 * 发生在 Rust 侧，见 lib/api.ts 的说明。输入框在提交后立即清空。
 */
export function AccountsPanel(): React.JSX.Element {
  const [accounts, setAccounts] = useState<AccountView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [label, setLabel] = useState("");
  const [apiKey, setApiKey] = useState("");
  // 展开查看配额的账号 id（点名称切换）
  const [expanded, setExpanded] = useState<number | null>(null);

  const load = useCallback(async (): Promise<void> => {
    try {
      const data = await sendJson<{ accounts: AccountView[] }>("POST", "/api/accounts/list");
      setAccounts(data.accounts);
      setError(null);
    } catch (cause) {
      setAccounts(null);
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const submit = useCallback(
    async (event: React.FormEvent): Promise<void> => {
      event.preventDefault();
      if (busy) {
        return;
      }
      setBusy(true);
      setError(null);
      try {
        await addAccount(label, apiKey);
        // 提交成功后立刻清空密钥输入：明文不该在界面里多停留一秒
        setApiKey("");
        setLabel("");
        await load();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [busy, label, apiKey, load],
  );

  const toggle = useCallback(
    async (id: number, enabled: boolean): Promise<void> => {
      setBusy(true);
      try {
        await sendJson("PATCH", `/api/accounts/${id}`, { enabled });
        await load();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const remove = useCallback(
    async (id: number): Promise<void> => {
      setBusy(true);
      try {
        await sendJson("DELETE", `/api/accounts/${id}`);
        await load();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  return (
    <section className="panel">
      <h2 className="panel__title">账号管理</h2>

      <form className="form" onSubmit={(event) => void submit(event)}>
        <input
          className="form__input"
          placeholder="名称（例如 Go #1）"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          disabled={busy}
        />
        <input
          className="form__input form__input--wide"
          placeholder="Command Code API key（user_ 开头）"
          type="password"
          value={apiKey}
          onChange={(event) => setApiKey(event.target.value)}
          disabled={busy}
        />
        <button type="submit" disabled={busy || label.trim() === "" || apiKey.trim() === ""}>
          添加
        </button>
      </form>

      {error !== null && <p className="panel__hint panel__hint--err">操作失败：{error}</p>}

      {accounts === null ? (
        <p className="panel__hint">加载中…</p>
      ) : accounts.length === 0 ? (
        <div className="onboarding-guide">
          <h3>欢迎使用 Command Code Desktop</h3>
          <p>当前未配置账号。只需简单配置，即可开启多账号自动轮换与额度实时监控：</p>
          <div className="onboarding-steps">
            <div className="onboarding-step">
              <div className="step-num">1</div>
              <div className="step-content">
                <strong>获取 API Key</strong>
                <p>从 Command Code 控制台复制 <code>user_</code> 开头的 API Key</p>
              </div>
            </div>
            <div className="onboarding-step">
              <div className="step-num">2</div>
              <div className="step-content">
                <strong>添加账号</strong>
                <p>在上方表单输入别名与密钥，点击“添加”完成本地安全存储</p>
              </div>
            </div>
            <div className="onboarding-step">
              <div className="step-num">3</div>
              <div className="step-content">
                <strong>接入开发工具</strong>
                <p>将客户端（Cursor、VS Code）的 Base URL 指向 <code>http://127.0.0.1:3050/v1</code></p>
              </div>
            </div>
          </div>
        </div>
      ) : (
        <table>
          <thead>
            <tr>
              <th>名称</th>
              <th>密钥提示</th>
              <th>状态与告警</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {accounts.map((account) => {
              const alerts = getAccountAlerts(account.quota);
              return (
                <tr key={account.id}>
                  <td>
                    {/* 点名称展开配额：配额数据较宽，塞进表格列会挤坏其他列 */}
                    <button
                      type="button"
                      className="link"
                      onClick={() => setExpanded(expanded === account.id ? null : account.id)}
                    >
                      {expanded === account.id ? "▾" : "▸"} {account.label}
                    </button>
                  </td>
                  <td><code>{account.key_hint}</code></td>
                  <td>
                    <span className="account-status-text">
                      {account.last_error ?? (account.enabled ? "正常服务" : "已停用")}
                    </span>
                    {alerts.map((alert, aIdx) => (
                      <span key={aIdx} className={`badge badge--alert badge--${alert.type}`}>
                        {alert.text}
                      </span>
                    ))}
                  </td>
                  <td className="num">
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => void toggle(account.id, !account.enabled)}
                    >
                      {account.enabled ? "停用" : "启用"}
                    </button>{" "}
                    <button type="button" disabled={busy} onClick={() => void remove(account.id)}>
                      删除
                    </button>
                  </td>
                </tr>
              );
            })}
            {expanded !== null && (
              <tr>
                <td colSpan={4}>
                  <QuotaCard quota={accounts.find((a) => a.id === expanded)?.quota} />
                </td>
              </tr>
            )}
          </tbody>
        </table>
      )}
    </section>
  );
}
