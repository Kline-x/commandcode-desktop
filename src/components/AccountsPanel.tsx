import { useCallback, useEffect, useState } from "react";

import { sendJson } from "../lib/api";

/** 一个账号在控制面上的形状。 */
interface AccountView {
  id: number;
  label: string;
  key_hint: string;
  enabled: boolean;
  plan_id: string | null;
  last_error: string | null;
  last_checked_ms: number | null;
}

/**
 * 账号面板。
 *
 * 额度进度条在 Phase 3 接入控制 API 后补齐；当前先展示账号清单与错误状态，
 * 让「代理有哪些账号可用」这件事在界面上可见。
 */
export function AccountsPanel(): React.JSX.Element {
  const [accounts, setAccounts] = useState<AccountView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

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

  return (
    <section className="panel">
      <h2 className="panel__title">账号</h2>
      {error !== null && <p className="panel__hint">读取失败：{error}</p>}
      {accounts === null ? (
        <p className="panel__hint">加载中…</p>
      ) : accounts.length === 0 ? (
        <p className="panel__hint">
          还没有账号。添加一个 Command Code API key（user_ 开头）后即可开始使用。
        </p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>名称</th>
              <th>密钥</th>
              <th>套餐</th>
              <th>状态</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {accounts.map((account) => (
              <tr key={account.id}>
                <td>{account.label}</td>
                <td>{account.key_hint}</td>
                <td>{account.plan_id ?? "—"}</td>
                <td>{account.last_error ?? (account.enabled ? "可用" : "已停用")}</td>
                <td className="num">
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => void toggle(account.id, !account.enabled)}
                  >
                    {account.enabled ? "停用" : "启用"}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
