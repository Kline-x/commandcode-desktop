import { useState } from "react";

export interface WindowUsage {
  used: number;
  cap: number;
  exceeded: boolean;
  reset_at_ms: number;
}

export interface MonthlyQuota {
  used: number;
  cap: number;
  reset_at_ms: number;
  derived: boolean;
}

export interface Credits {
  monthly: number;
  purchased: number;
  free: number;
  plan_id?: string | null;
}

export interface QuotaSnapshot {
  identity?: { id: string; label: string; org_id: string } | null;
  credits?: Credits | null;
  five_hour?: WindowUsage | null;
  weekly?: WindowUsage | null;
  monthly?: MonthlyQuota | null;
  plan_id?: string | null;
  plan_status?: string | null;
  period_end_ms?: number | null;
  alerts?: { kind?: string; message?: string }[] | string[] | null;
  last_error?: string | null;
}

export interface AccountView {
  id: number;
  label: string;
  key_hint: string;
  enabled: boolean;
  quota: unknown;
  last_error: string | null;
  last_checked_ms: number | null;
}

function fmtMoney(v: number | null | undefined): string {
  if (v == null || isNaN(v)) return "—";
  return `$${Number.isInteger(v) ? String(v) : v.toFixed(1)}`;
}

function formatDuration(seconds: number | null | undefined): string {
  if (seconds == null || isNaN(seconds) || seconds <= 0) return "";
  const s = Math.max(0, Math.floor(seconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}天 ${h}小时`;
  if (h > 0) return `${h}小时 ${m}分钟`;
  return `${m}分钟`;
}

function pctColor(usedPct: number): string {
  if (usedPct >= 90) return "var(--err)";
  if (usedPct >= 75) return "var(--warn)";
  if (usedPct >= 50) return "var(--warn)";
  return "var(--ok)";
}

interface WindowProps {
  label: string;
  w: { used: number; cap: number; exceeded?: boolean; reset_at_ms?: number };
  note?: string;
}

function WindowBar({ label, w, note = "" }: WindowProps): React.JSX.Element | null {
  if (!w || !w.cap || w.cap <= 0) return null;
  const usedPct = w.cap > 0 ? Math.min((w.used / w.cap) * 100, 100) : 0;
  const leftPct = Math.max(100 - usedPct, 0);
  const resetSec =
    w.reset_at_ms && w.reset_at_ms > 0 ? Math.floor((w.reset_at_ms - Date.now()) / 1000) : null;
  const resetText = !w.reset_at_ms || w.reset_at_ms <= 0
    ? "暂未使用，无重置点"
    : resetSec != null && resetSec > 0
    ? `重置于 ${formatDuration(resetSec)} 后`
    : "已重置";

  const color = pctColor(usedPct);

  return (
    <div className="window">
      <div className="window-header">
        <span className="window-label">{label}</span>
        <span className="window-pct" style={{ color }}>
          剩余 {leftPct.toFixed(1)}%
        </span>
      </div>
      <div className="bar-bg">
        <div className="bar-fill" style={{ width: `${leftPct}%`, background: color }} />
      </div>
      <div className="window-detail">
        <span>
          已用 {fmtMoney(w.used)} / {fmtMoney(w.cap)}
          {w.exceeded ? <span className="warn"> 已用满</span> : ""}
        </span>
        <span className="left" style={{ color }}>
          剩余 {fmtMoney(Math.max(w.cap - w.used, 0))}
        </span>
      </div>
      <div className="window-detail" style={{ marginTop: 2 }}>
        <span>
          {resetText}
          {note ? <span className="derived"> {note}</span> : ""}
        </span>
      </div>
    </div>
  );
}

export function AccountCard({
  account,
  busy,
  onToggle,
  onRemove,
  onRename,
}: {
  account: AccountView;
  busy: boolean;
  onToggle: (id: number, enabled: boolean) => Promise<void>;
  onRemove: (id: number) => Promise<void>;
  onRename: (id: number, label: string) => Promise<void>;
}): React.JSX.Element {
  const [expanded, setExpanded] = useState(false);
  const [editing, setEditing] = useState(false);
  const [editLabel, setEditLabel] = useState(account.label);

  const snapshot = account.quota as QuotaSnapshot | null;
  const credits = snapshot?.credits;
  const fiveHour = snapshot?.five_hour;
  const weekly = snapshot?.weekly;
  const monthly = snapshot?.monthly;

  const planName = snapshot?.plan_id;
  const isBlocked = fiveHour?.exceeded || weekly?.exceeded;
  const blockedName = fiveHour?.exceeded ? "5小时窗口" : weekly?.exceeded ? "周限额" : "";

  const lastCheckedText = account.last_checked_ms
    ? new Date(account.last_checked_ms).toLocaleString("zh-CN")
    : "从未刷新";

  const handleFinishEdit = async () => {
    setEditing(false);
    if (editLabel.trim() && editLabel.trim() !== account.label) {
      await onRename(account.id, editLabel.trim());
    } else {
      setEditLabel(account.label);
    }
  };

  return (
    <div className={`acc-card ${!account.enabled ? "acc-card--disabled" : ""}`}>
      <div className="acc-head">
        <div className="acc-title">
          {editing ? (
            <input
              className="acc-label-edit"
              value={editLabel}
              autoFocus
              onChange={(e) => setEditLabel(e.target.value)}
              onBlur={() => void handleFinishEdit()}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleFinishEdit();
                if (e.key === "Escape") {
                  setEditLabel(account.label);
                  setEditing(false);
                }
              }}
            />
          ) : (
            <span
              className="acc-label"
              onClick={() => setEditing(true)}
              title="点击修改备注"
            >
              {account.label || `#${account.id}`}
            </span>
          )}

          {planName && <span className="badge">{planName}</span>}
          {isBlocked && <span className="badge err">当前被{blockedName}拦截</span>}
          {!account.enabled && <span className="badge warn">已停用</span>}
          {account.last_error && <span className="badge err" title={account.last_error}>上次出错</span>}
          <span className="acc-masked">{account.key_hint}</span>
        </div>

        <div className="acc-actions">
          <button
            type="button"
            className="mini-btn"
            onClick={() => setExpanded(!expanded)}
          >
            {expanded ? "收起" : "详情"}
          </button>
          <button
            type="button"
            className="mini-btn"
            disabled={busy}
            onClick={() => void onToggle(account.id, !account.enabled)}
          >
            {account.enabled ? "停用" : "启用"}
          </button>
          <button
            type="button"
            className="mini-btn danger"
            disabled={busy}
            onClick={() => {
              if (window.confirm(`确定删除账号「${account.label}」吗？`)) {
                void onRemove(account.id);
              }
            }}
          >
            删除
          </button>
        </div>
      </div>

      <div className="bal-row">
        <span className="bal">
          月度剩余 <b>{fmtMoney(credits?.monthly)}</b>
        </span>
        <span className="bal">
          充值余额 <b>{fmtMoney(credits?.purchased)}</b>
        </span>
        <span className="bal">
          免费额度 <b>{fmtMoney(credits?.free)}</b>
        </span>
      </div>

      {fiveHour && <WindowBar label="5 小时滚动" w={fiveHour} />}
      {weekly && <WindowBar label="周限额" w={weekly} />}
      {monthly && (
        <WindowBar
          label="月度额度"
          w={monthly}
          note={monthly.derived ? "上限按套餐推算" : ""}
        />
      )}

      {expanded && (
        <div className="acc-detail">
          <div className="stat">
            <span>账号身份</span>
            <b>{snapshot?.identity?.label || snapshot?.identity?.id || "—"}</b>
          </div>
          <div className="stat">
            <span>组织标识</span>
            <b>{snapshot?.identity?.org_id || "—"}</b>
          </div>
          <div className="stat">
            <span>套餐类型</span>
            <b>{snapshot?.plan_id || "—"}</b>
          </div>
          <div className="stat">
            <span>订阅状态</span>
            <b>{snapshot?.plan_status || (account.enabled ? "正常服务" : "已停用")}</b>
          </div>
          {snapshot?.period_end_ms ? (
            <div className="stat wide">
              <span>计费周期结束</span>
              <b>{new Date(snapshot.period_end_ms).toLocaleString("zh-CN")}</b>
            </div>
          ) : null}
          {account.last_error && (
            <div className="stat wide">
              <span>错误诊断</span>
              <b style={{ color: "var(--err)" }}>{account.last_error}</b>
            </div>
          )}
        </div>
      )}

      <div className="acc-foot">
        上次刷新: {lastCheckedText}
        {account.last_error && <span className="err"> · {account.last_error}</span>}
      </div>
    </div>
  );
}
