import { useCallback, useEffect, useState } from "react";

import { addAccount, refreshAccounts, sendJson, updateAccount } from "../lib/api";
import { AccountCard, type AccountView } from "./AccountCard";

/**
 * 账号面板：移植自 commandcode-usage 的卡片网格与额度监控系统。
 *
 * 账号以卡片矩阵形式平铺，直观展示 5 小时滚动窗口、每周限额、月度推算额度与信用余额。
 */
export function AccountsPanel(): React.JSX.Element {
  const [accounts, setAccounts] = useState<AccountView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const [label, setLabel] = useState("");
  const [apiKey, setApiKey] = useState("");

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
      const trimmedKey = apiKey.trim();
      const trimmedLabel = label.trim();
      if (!trimmedKey) {
        setError("请先填入 API 密钥。");
        return;
      }
      if (!/^[\x21-\x7e]+$/.test(trimmedKey)) {
        setError("密钥格式不对：检测到中文或全角字符，请重新复制粘贴。");
        return;
      }

      setBusy(true);
      setError(null);
      try {
        await addAccount(trimmedLabel, trimmedKey);
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
        await updateAccount(id, { enabled });
        await load();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const rename = useCallback(
    async (id: number, newLabel: string): Promise<void> => {
      setBusy(true);
      try {
        await updateAccount(id, { label: newLabel });
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

  const handleRefreshAll = useCallback(async (): Promise<void> => {
    setRefreshing(true);
    try {
      await refreshAccounts();
      // 等待 1 秒使后端探测完成
      await new Promise((r) => setTimeout(r, 1200));
      await load();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setRefreshing(false);
    }
  }, [load]);

  const activeCount = accounts?.filter((a) => a.enabled).length ?? 0;
  const errCount = accounts?.filter((a) => Boolean(a.last_error)).length ?? 0;

  return (
    <section className="panel" style={{ padding: "20px 24px" }}>
      <div style={{ marginBottom: 16 }}>
        <h2 style={{ fontSize: 18, margin: "0 0 4px", color: "#f0f6fc" }}>
          Command Code 多账号额度面板
        </h2>
        <p style={{ fontSize: 13, margin: 0, color: "#8b949e" }}>
          账号存于本地私有安全存储，随时刷新 5 小时滚动 / 周窗口与信用余额
        </p>
      </div>

      <form className="form" onSubmit={(event) => void submit(event)} style={{ marginBottom: 8 }}>
        <input
          className="form__input"
          style={{ maxWidth: 200, fontFamily: "inherit" }}
          placeholder="备注名（可留空，自动取名）"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          disabled={busy}
        />
        <input
          className="form__input form__input--wide"
          placeholder="API 密钥（从 commandcode.ai/settings 获取）"
          type="password"
          value={apiKey}
          onChange={(event) => setApiKey(event.target.value)}
          disabled={busy}
        />
        <button type="submit" disabled={busy || apiKey.trim() === ""}>
          {busy ? "处理中…" : "添加"}
        </button>
      </form>

      <p style={{ fontSize: 12, color: "#8b949e", margin: "0 0 16px" }}>
        添加时自动验证密钥有效性；备注随时可改（点击卡片上的名字）。密钥仅存于本地安全存储。
      </p>

      {error !== null && (
        <div
          style={{
            background: "#3d1519",
            border: "1px solid #da3633",
            borderRadius: 8,
            padding: "10px 14px",
            fontSize: 13,
            color: "#f85149",
            marginBottom: 16,
          }}
        >
          {error}
        </div>
      )}

      {accounts !== null && accounts.length > 0 && (
        <div className="acc-toolbar">
          <span className="count">
            {accounts.length} 个账号 · {activeCount} 个服务中
            {errCount > 0 && <span style={{ color: "#f85149" }}> · {errCount} 个上次出错</span>}
          </span>
          <button
            type="button"
            className="mini-btn"
            style={{ padding: "5px 12px", fontSize: 12 }}
            disabled={refreshing}
            onClick={() => void handleRefreshAll()}
          >
            {refreshing ? "刷新中…" : "全部刷新"}
          </button>
        </div>
      )}

      {accounts === null ? (
        <div style={{ textAlign: "center", color: "#8b949e", padding: "40px 0" }}>加载中…</div>
      ) : accounts.length === 0 ? (
        <div className="onboarding-guide">
          <h3>欢迎使用 Command Code Desktop</h3>
          <p>当前未配置账号。在上方填入 API 密钥，即可添加第一个账号并开启多账号自动轮换：</p>
          <div className="onboarding-steps">
            <div className="onboarding-step">
              <div className="step-num">1</div>
              <div className="step-content">
                <strong>获取 API Key</strong>
                <p>
                  从 Command Code 控制台复制 <code>user_</code> 开头的 API Key
                </p>
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
                <p>
                  将客户端（Cursor、VS Code）的 Base URL 指向 <code>http://127.0.0.1:3050/v1</code>
                </p>
              </div>
            </div>
          </div>
        </div>
      ) : (
        <div className="acc-grid">
          {accounts.map((account) => (
            <AccountCard
              key={account.id}
              account={account}
              busy={busy}
              onToggle={toggle}
              onRemove={remove}
              onRename={rename}
            />
          ))}
        </div>
      )}
    </section>
  );
}
