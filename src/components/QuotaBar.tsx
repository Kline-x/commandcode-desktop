/** 一条额度进度条。 */
export interface WindowUsage {
  used: number;
  cap: number;
  exceeded: boolean;
  reset_at_ms: number;
}

/** 配额快照（与 cc-server 的 quota::QuotaSnapshot 对应，只取 UI 需要的字段）。 */
export interface QuotaSnapshot {
  credits?: { monthly: number; purchased: number; free: number } | null;
  five_hour?: WindowUsage | null;
  weekly?: WindowUsage | null;
  monthly?: { used: number; cap: number; reset_at_ms: number; derived: boolean } | null;
  plan_id?: string | null;
  last_error?: string | null;
}

/** 把 epoch 毫秒格式化成「X 小时 Y 分后」。 */
function untilReset(resetAtMs: number): string {
  const diff = resetAtMs - Date.now();
  if (diff <= 0) {
    return "已重置";
  }
  const minutes = Math.floor(diff / 60_000);
  const hours = Math.floor(minutes / 60);
  if (hours > 0) {
    return `${hours} 小时 ${minutes % 60} 分后重置`;
  }
  return `${minutes} 分后重置`;
}

/**
 * 用已用比例决定颜色。
 *
 * 阈值与参考面板一致（50% 黄 / 75% 橙 / 90% 红）——用颜色表达紧迫度，
 * 比让用户自己算百分比快得多。
 */
function levelClass(ratio: number): string {
  if (ratio >= 0.9) {
    return "quota__bar--danger";
  }
  if (ratio >= 0.75) {
    return "quota__bar--high";
  }
  if (ratio >= 0.5) {
    return "quota__bar--mid";
  }
  return "quota__bar--low";
}

/** 单条窗口进度。 */
function Window({
  label,
  usage,
}: {
  label: string;
  usage: WindowUsage;
}): React.JSX.Element {
  // cap 为 0 时不做除法（上游可能返回 0，表示该窗口不适用）
  const ratio = usage.cap > 0 ? Math.min(usage.used / usage.cap, 1) : 0;
  return (
    <div className="quota">
      <div className="quota__head">
        <span className="quota__label">{label}</span>
        <span className="quota__value">
          {usage.used.toFixed(2)} / {usage.cap.toFixed(2)}
          {usage.exceeded && <strong className="quota__exceeded"> 已超限</strong>}
        </span>
      </div>
      <div className="quota__track">
        <div className={`quota__bar ${levelClass(ratio)}`} style={{ width: `${ratio * 100}%` }} />
      </div>
      <div className="quota__hint">{untilReset(usage.reset_at_ms)}</div>
    </div>
  );
}

/**
 * 配额卡片：5 小时滚动窗口 + 每周窗口 + 月度余额。
 *
 * 数据来自后端的配额轮询器（45s 一次），经控制面的 /api/accounts 返回。
 * 这里只负责展示——派生逻辑（月度 cap 等）在 Rust 侧完成，前端不重复实现。
 */
export function QuotaCard({ quota }: { quota: unknown }): React.JSX.Element | null {
  if (quota === null || typeof quota !== "object") {
    return null;
  }
  const snapshot = quota as QuotaSnapshot;
  const hasWindows = snapshot.five_hour != null || snapshot.weekly != null;
  const credits = snapshot.credits;
  if (!hasWindows && credits == null) {
    return null;
  }

  return (
    <div className="quota-card">
      {snapshot.plan_id != null && <div className="quota-card__plan">套餐：{snapshot.plan_id}</div>}
      {snapshot.five_hour != null && <Window label="5 小时窗口" usage={snapshot.five_hour} />}
      {snapshot.weekly != null && <Window label="每周窗口" usage={snapshot.weekly} />}
      {credits != null && (
        <div className="quota__hint">
          月度余额 {credits.monthly.toFixed(2)}
          {credits.purchased > 0 && ` · 已购 ${credits.purchased.toFixed(2)}`}
          {credits.free > 0 && ` · 赠送 ${credits.free.toFixed(2)}`}
        </div>
      )}
      {snapshot.last_error != null && (
        <div className="quota__hint quota__hint--err">刷新失败：{snapshot.last_error}</div>
      )}
    </div>
  );
}

export interface AccountAlert {
  text: string;
  type: "err" | "warn" | "ok";
}

export function getAccountAlerts(quota: unknown): AccountAlert[] {
  if (quota == null || typeof quota !== "object") {
    return [];
  }
  const alerts: AccountAlert[] = [];
  const snapshot = quota as QuotaSnapshot;
  if (snapshot.five_hour?.exceeded) {
    alerts.push({ text: "5h 超限", type: "err" });
  }
  if (snapshot.weekly?.exceeded) {
    alerts.push({ text: "周超限", type: "err" });
  }
  if (snapshot.credits) {
    const total =
      (snapshot.credits.monthly ?? 0) +
      (snapshot.credits.purchased ?? 0) +
      (snapshot.credits.free ?? 0);
    if (total < 1.0) {
      alerts.push({ text: "低余额", type: "warn" });
    }
  }
  return alerts;
}
