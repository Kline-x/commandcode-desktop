import { useCallback, useEffect, useState } from "react";
import { getRules, sendJson, updateRules, type RouteRule } from "../lib/api";

interface AccountOption {
  id: number;
  label: string;
  key_hint: string;
}

export function RulesPanel(): React.JSX.Element {
  const [rules, setRules] = useState<RouteRule[]>([]);
  const [accounts, setAccounts] = useState<AccountOption[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [successMsg, setSuccessMsg] = useState<string | null>(null);

  // 新增规则表单
  const [newModels, setNewModels] = useState("");
  const [newAccountId, setNewAccountId] = useState("");

  const loadData = useCallback(async () => {
    try {
      setLoading(true);
      setError(null);
      const [fetchedRules, accountsResp] = await Promise.all([
        getRules(),
        sendJson<{ accounts: AccountOption[] }>("POST", "/api/accounts/list", {}),
      ]);
      setRules(fetchedRules);
      setAccounts(accountsResp.accounts || []);
      if (accountsResp.accounts && accountsResp.accounts.length > 0 && !newAccountId) {
        const first = accountsResp.accounts[0];
        if (first) {
          setNewAccountId(String(first.id));
        }
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [newAccountId]);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  const handleAddRule = (e: React.FormEvent) => {
    e.preventDefault();
    const patterns = newModels
      .split(/[,，\s]+/)
      .map((s) => s.trim())
      .filter(Boolean);

    if (patterns.length === 0) {
      setError("请输入至少一个模型名称或通配符（如 claude-*）");
      return;
    }
    if (!newAccountId) {
      setError("请选择目标账号");
      return;
    }

    const newRule: RouteRule = {
      id: Date.now(), // 临时前端 key
      models: patterns,
      account_id: newAccountId,
    };
    setRules([...rules, newRule]);
    setNewModels("");
    setError(null);
  };

  const handleDeleteRule = (index: number) => {
    setRules(rules.filter((_, i) => i !== index));
  };

  const handleMoveUp = (index: number) => {
    if (index <= 0) return;
    const copy = [...rules];
    const prev = copy[index - 1];
    const curr = copy[index];
    if (prev !== undefined && curr !== undefined) {
      copy[index - 1] = curr;
      copy[index] = prev;
      setRules(copy);
    }
  };

  const handleMoveDown = (index: number) => {
    if (index >= rules.length - 1) return;
    const copy = [...rules];
    const next = copy[index + 1];
    const curr = copy[index];
    if (next !== undefined && curr !== undefined) {
      copy[index + 1] = curr;
      copy[index] = next;
      setRules(copy);
    }
  };

  const handleSave = async () => {
    try {
      setSaving(true);
      setError(null);
      const payload = rules.map((r) => ({
        models: r.models,
        account_id: String(r.account_id),
      }));
      await updateRules(payload);
      setSuccessMsg("路由规则已保存并热重载生效！");
      setTimeout(() => setSuccessMsg(null), 3000);
      await loadData();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  const getAccountLabel = (accId: string) => {
    const acc = accounts.find((a) => String(a.id) === String(accId) || a.label === accId);
    return acc ? `${acc.label} (${acc.key_hint})` : `账号 #${accId}`;
  };

  return (
    <section className="panel rules-panel">
      <div className="panel__header">
        <div>
          <h2>模型路由规则</h2>
          <p className="panel__desc">
            请求匹配模型模式时优先分发至指定账号。越靠前的规则优先级越高；未匹配的请求自动按默认账号池轮换。
          </p>
        </div>
        <button
          className="btn btn--primary"
          onClick={() => void handleSave()}
          disabled={saving || loading}
        >
          {saving ? "保存中…" : "保存规则"}
        </button>
      </div>

      {error && <div className="notice notice--err">{error}</div>}
      {successMsg && <div className="notice notice--ok">{successMsg}</div>}

      {/* 新增规则区域 */}
      <form className="rules-form" onSubmit={handleAddRule}>
        <div className="form-group" style={{ flex: 2 }}>
          <label>模型名称或通配符</label>
          <input
            type="text"
            placeholder="如: claude-3-7-sonnet*, deepseek-*"
            value={newModels}
            onChange={(e) => setNewModels(e.target.value)}
          />
        </div>
        <div className="form-group" style={{ flex: 1 }}>
          <label>目标路由账号</label>
          <select
            value={newAccountId}
            onChange={(e) => setNewAccountId(e.target.value)}
            disabled={accounts.length === 0}
          >
            {accounts.length === 0 ? (
              <option value="">暂无可用账号</option>
            ) : (
              accounts.map((a) => (
                <option key={a.id} value={String(a.id)}>
                  {a.label} ({a.key_hint})
                </option>
              ))
            )}
          </select>
        </div>
        <div className="form-group" style={{ alignSelf: "flex-end" }}>
          <button type="submit" className="btn btn--secondary" disabled={accounts.length === 0}>
            + 添加规则
          </button>
        </div>
      </form>

      {/* 规则列表 */}
      {loading ? (
        <div className="panel__empty">加载规则中…</div>
      ) : rules.length === 0 ? (
        <div className="panel__empty">
          当前暂无路由规则。所有请求将遵循账号池轮换策略。
        </div>
      ) : (
        <table className="table rules-table">
          <thead>
            <tr>
              <th style={{ width: "60px" }}>优先级</th>
              <th>模型匹配模式</th>
              <th>绑定账号</th>
              <th style={{ width: "160px", textAlign: "right" }}>操作</th>
            </tr>
          </thead>
          <tbody>
            {rules.map((rule, idx) => (
              <tr key={rule.id || idx}>
                <td>
                  <span className="badge badge--num">#{idx + 1}</span>
                </td>
                <td>
                  <div className="rule-models">
                    {rule.models.map((m, mIdx) => (
                      <span key={mIdx} className="badge badge--model">
                        {m}
                      </span>
                    ))}
                  </div>
                </td>
                <td>
                  <span className="rule-account">{getAccountLabel(rule.account_id)}</span>
                </td>
                <td style={{ textAlign: "right" }}>
                  <button
                    className="btn btn--sm btn--icon"
                    onClick={() => handleMoveUp(idx)}
                    disabled={idx === 0}
                    title="上移"
                  >
                    ↑
                  </button>
                  <button
                    className="btn btn--sm btn--icon"
                    onClick={() => handleMoveDown(idx)}
                    disabled={idx === rules.length - 1}
                    title="下移"
                  >
                    ↓
                  </button>
                  <button
                    className="btn btn--sm btn--danger"
                    onClick={() => handleDeleteRule(idx)}
                    title="删除规则"
                  >
                    删除
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
