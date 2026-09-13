//! Command Code 配额端点的**纯解析逻辑**（不做网络请求，无 I/O）。
//!
//! 数据源是 4 个上游端点，字段形状见 docs/PROTOCOL.md 第 7 节：
//!
//! - `GET /alpha/whoami` — 账号身份（userName / name / id，以及可选 org.id）
//! - `GET /alpha/billing/credits` — 余额 + 5 小时 / 周窗口 + 限流标记
//! - `GET /alpha/billing/subscriptions[?orgId=…]` — 套餐、状态、账期结束时间
//! - `GET /alpha/usage/summary` — 计费周期汇总（requests / success rate / tokens / credits）
//!
//! 解析规则逐一对应 third_party/usage-worker.js 的 `fetchReport` / `normalizeWindow` /
//! `planInfo`（MIT，见 THIRD_PARTY.md），并保留其防御性：
//!
//! - **字段缺失降级而不是报错**：上游接口未公开，字段会漂移；宁可少报一个数字，
//!   也不能让整个面板因为一个字段变形而白屏。见 docs/PROTOCOL.md 第 7 节。
//! - **某个端点失败不影响其余端点**：四个端点独立降级，失败原因汇总进
//!   [`QuotaSnapshot::last_error`]。见 docs/PROTOCOL.md 第 7 节「某个端点失败不影响其余端点」。
//! - **探针失败不得改变账号池状态**（docs/PROTOCOL.md 第 7 节）：因此本模块只产出一份
//!   只读的 [`QuotaSnapshot`]，不触碰 `"crate::pool::AccountState"`，也不读系统时间。
//! - **月度额度是派生值**：cap 来自套餐映射 [`plan_monthly_cap`]，`used = cap − 余额`，
//!   重置时间取计费周期结束；未知套餐时不做减法（`derived = false`），只报告余额。
//!   见 docs/PROTOCOL.md 第 7 节末段。
//!
//! 时间统一用 `i64` 毫秒（Unix epoch），带单位的量写进名字（`reset_at_ms` 等）；
//! 需要「现在」的地方一律由参数注入 `now_ms: i64`，不在函数内部读系统时间
//! （docs/STYLE.md 第 2.3 节，全仓库硬性约定，pool.rs 已落地）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 可用余额低于该值时告警的兜底阈值（credit）。
///
/// 上游通常会在同一响应里给出 `belowThreshold`，该布尔值优先；只有在它缺失时
/// 才用这个数值阈值推算，避免多报一条 LowBalance 把真正的窗口耗尽淹没掉。
pub const LOW_BALANCE_THRESHOLD: f64 = 5.0;

/// 已知套餐 → 月度信用额（credit）映射：`(前缀, 展示名, 月度额度)`。
///
/// 数值来自社区 CLI 的 planId 映射，移植自 third_party/usage-worker.js 的
/// `KNOWN_PLANS`。**最长前缀优先**：`individual-pro-v1` 必须胜过 `individual-pro`，
/// 否则 Pro v1（80）会被误判成 Pro（30）。
///
/// 顺序即匹配优先级：`individual-pro` 是 `individual-pro-v1` 的前缀，因此必须排在后面。
/// 修改本表请同步 third_party 参考实现，避免两侧面板数字不一致。
const KNOWN_PLANS: &[(&str, &str, f64)] = &[
    ("individual-provider", "Provider", 15.0),
    ("individual-goat", "GOAT", 70.0),
    ("individual-ultra", "Ultra", 300.0),
    ("individual-max", "Max", 150.0),
    ("teams-pro", "Teams Pro", 40.0),
    ("individual-pro-v1", "Pro", 80.0),
    ("individual-go", "Go", 10.0),
    ("individual-pro", "Pro", 30.0),
];

/// 窗口对象键名无法识别时的稳定返回值（不分配），用于区分「找不到」与「空键名」。
const UNKNOWN_WINDOW: &str = "unknown";

/// 判断该 JSON 值是否为「对象」（不是数组、不是 null）。
///
/// 上游的 credits / subscription 既可能直接挂在根上，也可能包在 `data` 里，
/// 还可能是数组或字符串——用数组下标访问它们会静默取到 null，因此先做形状判定。
fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// 从对象里取子对象；不是对象则返回 None（降级，不报错）。
fn get_record<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|child| is_record(child))
}

/// 解析布尔标记：接受真布尔与字符串 `"true"` / `"false"`。
///
/// 上游不同接口对同一语义时而给布尔时而给字符串（usage-worker.js 的
/// `raw.exceeded === true || raw.exceeded === 'true'` 即为此），这里统一收口。
fn bool_flag(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// 把 JSON 值解析成数字：整数与浮点都要能解析。
///
/// 非数字（null / 字符串 / 数组）返回 None——由调用方决定降级值，
/// 这样「字段存在但类型漂移」与「字段缺失」走同一条降级路径。
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|parsed| parsed.is_finite()),
        _ => None,
    }
}

/// 在若干候选键里取第一个能解析成数字的值。
///
/// 上游字段时而驼峰时而下划线（`monthlyCredits` / `monthly_credits`），
/// 逐个别名尝试是参考实现既有的兼容策略。
fn number_any(record: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| record.get(*key).and_then(number))
}

/// 在若干候选键里取第一个非空字符串（空串视为缺失，避免把空串当有效标识）。
fn string_any(record: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| record.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 把「字符串或数字」的标识转成字符串（orgId / userId 两种类型都见过）。
fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()).filter(|text| !text.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 公历日期 → 距 1970-01-01 的天数（Howard Hinnant 的 `days_from_civil`）。
///
/// 与 error.rs 里的 `civil_from_days` 互为逆运算，不引入日期库（保持依赖精简）。
/// `month` 需在 1..=12、`day` 在 1..=31（调用方按 ISO 语法已校验）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 全程为 ASCII 数字的字符串转 `i64`；含其他字符（符号、空白、空串）返回 None。
fn ascii_digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>().ok()
}

/// 把 `.` 之后的小数秒换算成毫秒（截断到 3 位，`.5` 记作 500 ms）。
fn fraction_to_ms(text: &str) -> i64 {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return 0;
    }
    let head: String = digits.chars().take(3).collect();
    head.parse::<i64>()
        .map(|ms| ms * 10_i64.pow(3 - head.len() as u32))
        .unwrap_or(0)
}

/// 解析 `±HH:MM` / `±HHMM` / `±HH` 形式的时区偏移为分钟数；非法返回 None。
fn parse_offset_minutes(text: &str) -> Option<i64> {
    let sign = match text.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest = &text[1..];
    let (hours, minutes) = match rest.split_once(':') {
        Some((h, m)) => (ascii_digits(h)?, ascii_digits(m)?),
        None if rest.len() == 4 => (ascii_digits(&rest[..2])?, ascii_digits(&rest[2..])?),
        None if rest.len() == 2 => (ascii_digits(rest)?, 0),
        None => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// 解析 ISO8601 / RFC3339 时间戳为 epoch 毫秒；无法解析返回 None。
///
/// 支持上游实际会给出的形式：`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`（T 可为空格，
/// 小数分隔符可为 `.` 或 `,`）。带时区偏移会被换算成 UTC。
///
/// **有意的取舍**：闰秒（`:60`）与扩展年份不特判；无法解析时宁可不给 reset 时间，
/// 也不猜一个错误时刻——错误的 reset 时间会直接变成误导用户的重试等待。
fn parse_iso8601_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    // 语法最短长度：YYYY-MM-DDTHH:MM:SSZ = 20
    if text.len() < 20 {
        return None;
    }
    // 先剥离时区，再剥离小数秒：反过来会把 `+08:00` 当小数部分吞掉
    let (stamp, offset_minutes) =
        if let Some(stripped) = text.strip_suffix('Z').or_else(|| text.strip_suffix('z')) {
            (stripped, 0)
        } else {
            match text.rfind(['+', '-']) {
                // 日期部分的 `-` 出现在前 10 个字符内；更靠后的才是时区偏移
                Some(pos) if pos >= 10 => (&text[..pos], parse_offset_minutes(&text[pos..])?),
                _ => (text, 0),
            }
        };
    let (core, fraction_ms) = match stamp.find(['.', ',']) {
        Some(pos) => (&stamp[..pos], fraction_to_ms(&stamp[pos + 1..])),
        None => (stamp, 0),
    };
    let (date, time) = core.split_once(['T', 't', ' '])?;

    let first_dash = date.find('-')?;
    let second_dash = date[first_dash + 1..].find('-')? + first_dash + 1;
    let year = ascii_digits(&date[..first_dash])?;
    let month = ascii_digits(&date[first_dash + 1..second_dash])?;
    let day = ascii_digits(&date[second_dash + 1..])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut time_parts = time.splitn(3, ':');
    let hour = ascii_digits(time_parts.next()?)?;
    let minute = ascii_digits(time_parts.next()?)?;
    let second = ascii_digits(time_parts.next()?)?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let secs = days_from_civil(year, month, day)
        .saturating_mul(86_400)
        .saturating_add(hour * 3600 + minute * 60 + second)
        .saturating_sub(offset_minutes * 60);
    Some(secs.saturating_mul(1000).saturating_add(fraction_ms))
}

/// 把多种时间表示归一为 epoch 毫秒：数字（秒或毫秒）或 ISO 字符串。
///
/// 数字的判定沿用参考实现 `toEpochMs`：小于 `1e12` 视为秒级时间戳
/// （10 位秒 ≈ 2001–2286 年），否则视为毫秒。无法解析返回 None，
/// 由调用方降级为「重置时间未知」。
fn normalize_epoch_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => {
            let raw = n.as_f64().filter(|x| x.is_finite())?;
            if raw < 0.0 {
                return None;
            }
            if raw < 1_000_000_000_000.0 {
                Some((raw * 1000.0).round() as i64)
            } else {
                Some(raw.round() as i64)
            }
        }
        Value::String(text) => parse_iso8601_ms(text),
        _ => None,
    }
}

/// 在若干候选键里取第一个能解析成 epoch 毫秒的时间。
fn epoch_ms_any(record: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| record.get(*key).and_then(normalize_epoch_ms))
}

/// 账号身份（来自 `/alpha/whoami`）。见 docs/PROTOCOL.md 第 7 节。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    /// `user.id`；缺失或类型漂移时为 None。
    pub user_id: Option<String>,
    /// `user.name`（真实姓名或显示名）。
    pub user_name: Option<String>,
    /// 展示名：`userName` 非空用它，否则用 `name`，再否则用 `user_id`。
    ///
    /// 三者都没有时为空串——调用方应用掩码后的 API key 兜底，
    /// 绝不要在展示层回落到明文密钥（docs/STYLE.md 第 5 节红线）。
    pub label: String,
    /// `org.id`；订阅端点需要它作为 `?orgId=`（也可能是数字，一并转成字符串）。
    pub org_id: Option<String>,
}

impl AccountIdentity {
    /// 从 whoami 响应解析身份；字段缺失时降级而不是报错。
    ///
    /// 兼容三种包装：根上直接给 `user`、包在 `data.user` 里（参考实现的兜底分支）、
    /// 以及 `userName` / `username` 两种拼写。
    pub fn from_whoami(value: &Value) -> Self {
        let user = get_record(value, "user")
            .or_else(|| get_record(value, "data").and_then(|data| get_record(data, "user")));
        let user_id = user.and_then(|u| u.get("id")).and_then(id_string);
        let name = user.and_then(|u| string_any(u, &["name"]));
        let user_name = user.and_then(|u| string_any(u, &["userName", "username"]));
        let label = user_name
            .clone()
            .or_else(|| name.clone())
            .or_else(|| user_id.clone())
            .unwrap_or_default();
        let org_id = get_record(value, "org")
            .and_then(|org| org.get("id"))
            .and_then(id_string);
        Self {
            user_id,
            user_name,
            label,
            org_id,
        }
    }
}

/// 一个滚动窗口（5 小时 / 周）的用量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowUsage {
    /// 已用额度；缺失时降级为 0。
    pub used: f64,
    /// 窗口额度上限；缺失时降级为 0（此时不得用于推算「还剩多少」）。
    pub cap: f64,
    /// 是否已超限；显式标记优先，标记缺失时由 `used >= cap` 推算。
    pub exceeded: bool,
    /// 窗口重置时刻（epoch 毫秒）；未知为 0。
    pub reset_at_ms: i64,
}

impl WindowUsage {
    /// 从 `windowLimits.fiveHour` 这类窗口对象解析；不是对象则返回 None（视为该窗口缺失）。
    ///
    /// 字段别名与推算规则照搬参考实现 `normalizeWindow`：
    /// `used` 也见过 `usage` / `usedCredits`，`cap` 也见过 `limit` / `capCredits`。
    pub fn from_json(value: &Value) -> Option<Self> {
        if !is_record(value) {
            return None;
        }
        let used =
            number_any(value, &["used", "usage", "usedCredits", "used_credits"]).unwrap_or(0.0);
        let cap = number_any(value, &["cap", "limit", "capCredits"]).unwrap_or(0.0);
        // 显式标记优先：`exceeded` 为真字符串 "true" 时也算超限
        let flagged = bool_flag(value.get("exceeded"));
        let inferred = used > 0.0 && cap > 0.0 && used >= cap;
        let reset_at_ms = epoch_ms_any(value, &["resetAt", "reset_at", "resetsAt"]).unwrap_or(0);
        Some(Self {
            used,
            cap,
            exceeded: flagged || inferred,
            reset_at_ms,
        })
    }
}

/// 距窗口重置还剩多少毫秒；重置时间未知或已过期返回 0。
///
/// 时间由参数注入（docs/STYLE.md 第 2.3 节），因此等待时长可在不睡眠的前提下单测。
pub fn window_reset_wait_ms(window: &WindowUsage, now_ms: i64) -> i64 {
    if window.reset_at_ms <= 0 {
        return 0;
    }
    (window.reset_at_ms - now_ms).max(0)
}

/// 余额（来自 `/alpha/billing/credits` 的 `credits`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Credits {
    /// 套餐内含的月度剩余额度；缺失降级为 0。
    pub monthly: f64,
    /// 单独购买的额度；缺失降级为 0。
    pub purchased: f64,
    /// 赠送额度；缺失降级为 0。
    pub free: f64,
    /// 该响应自带的 planId；订阅端点失败时作为兜底（参考实现的 `_planIdFallback`）。
    pub plan_id: Option<String>,
}

impl Credits {
    /// 从 credits 对象解析；不是对象则返回 None。
    ///
    /// 字段名同时接受驼峰与下划线两种拼写。
    pub fn from_json(value: &Value) -> Option<Self> {
        if !is_record(value) {
            return None;
        }
        Some(Self {
            monthly: number_any(value, &["monthlyCredits", "monthly_credits"]).unwrap_or(0.0),
            purchased: number_any(value, &["purchasedCredits", "purchased_credits"]).unwrap_or(0.0),
            free: number_any(value, &["freeCredits", "free_credits"]).unwrap_or(0.0),
            plan_id: string_any(value, &["planId", "plan_id"]),
        })
    }
}

/// 月度额度视图。
///
/// **这是派生值**，不是上游直接给的字段（docs/PROTOCOL.md 第 7 节末段）：
///
/// - cap 来自套餐映射 [`plan_monthly_cap`]；
/// - `used = cap − credits.monthly`（余额）；
/// - 重置时间取 `subscriptions.currentPeriodEnd`。
///
/// 套餐未知或账期结束时间缺失时 `derived = false`，此时 [`MonthlyQuota::cap`] 为 0、
/// [`MonthlyQuota::used`] **承载的是剩余余额**（不要拿它算进度百分比），
/// 前端据此降级为「只显示余额」。见 third_party/usage-worker.js 的 `monthlyCredits` 注释。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthlyQuota {
    /// 已用额度；`derived = false` 时该字段承载的是剩余余额（仅用于展示）。
    pub used: f64,
    /// 月度额度上限；未知套餐为 0。
    pub cap: f64,
    /// 计费周期结束时刻（epoch 毫秒）；未知为 0。
    pub reset_at_ms: i64,
    /// 是否为「cap − 余额」推导出的可信值。
    pub derived: bool,
}

/// 配额告警：面板据此显示徽标（docs/PLAN.md 第 5 节）。
///
/// **为什么用带数据的枚举**：告警需要携带窗口名这类上下文；若只给标记，
/// 前端得回头再查一遍快照，容易与它看到的数据不一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuotaAlert {
    /// 某个滚动窗口已超限；参数是窗口在实际响应里的键名（如 `fiveHour`）。
    WindowExceeded(String),
    /// 剩余额度低于阈值：上游 `belowThreshold` 为真，或可用余额低于
    /// [`LOW_BALANCE_THRESHOLD`]。
    LowBalance,
    /// 订阅已设置为「账期结束不再续费」。
    CancelsAtPeriodEnd,
    /// 存在待生效的套餐变更（`pendingPhase` 非 null）。
    PendingPlanChange,
}

impl QuotaAlert {
    /// 稳定的告警类型标识（英文，供结构化日志与前端 i18n 查表）。
    ///
    /// 与展示文案分离：契约是标识，文案可以随时改（docs/STYLE.md 第 5 节）。
    pub fn kind(&self) -> &'static str {
        match self {
            QuotaAlert::WindowExceeded(_) => "window_exceeded",
            QuotaAlert::LowBalance => "low_balance",
            QuotaAlert::CancelsAtPeriodEnd => "cancels_at_period_end",
            QuotaAlert::PendingPlanChange => "pending_plan_change",
        }
    }

    /// 面向用户的中文告警文案（错误信息三段式：做什么 / 为什么 / 怎么办）。
    pub fn message(&self) -> String {
        match self {
            QuotaAlert::WindowExceeded(window) => {
                format!("{window} 用量窗口已超限——窗口重置后自动恢复，或切换到其他账号")
            }
            QuotaAlert::LowBalance => {
                "剩余额度不足——请充值或升级套餐，否则请求会被上游拒绝".to_string()
            }
            QuotaAlert::CancelsAtPeriodEnd => {
                "订阅将在本计费周期结束时取消——到期后配额停止刷新".to_string()
            }
            QuotaAlert::PendingPlanChange => {
                "套餐变更待生效——生效时间以计费周期结束为准".to_string()
            }
        }
    }
}

/// 四端点聚合后的配额快照（面板的唯一数据源）。
///
/// 字段全部可缺省：某个端点失败时其余字段仍然可用（docs/PROTOCOL.md 第 7 节）。
/// 本结构是**只读结果**，探针失败不影响账号池状态。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// whoami 解析出的身份；whoami 失败时保持默认值（label 为空串）。
    pub identity: AccountIdentity,
    /// billing/credits 的余额；该端点失败或响应里没有 credits 时为 None。
    pub credits: Option<Credits>,
    /// 5 小时滚动窗口。
    pub five_hour: Option<WindowUsage>,
    /// 周滚动窗口。
    pub weekly: Option<WindowUsage>,
    /// 派生出的月度额度。
    pub monthly: Option<MonthlyQuota>,
    /// 套餐 id；优先取订阅端点，缺失时回落到 credits 里的 planId。
    pub plan_id: Option<String>,
    /// 订阅状态（active / trialing / past_due 等，原样透传）。
    pub plan_status: Option<String>,
    /// 计费周期结束时刻（epoch 毫秒）。
    pub period_end_ms: Option<i64>,
    /// 告警列表；顺序稳定（窗口 → 余额 → 订阅），便于前端做 diff。
    pub alerts: Vec<QuotaAlert>,
    /// 失败端点的汇总（**面向诊断日志**，面向用户的文案另行克制处理）。
    pub last_error: Option<String>,
}

/// 输入给 [`build_snapshot`] 的四个端点结果。
///
/// `Err` 表示该端点网络/HTTP 失败，`Ok(Value)` 表示拿到了 JSON（内容仍可能是任何形状）；
/// `None` 表示本次没有发起该请求。三种情况都必须安全降级。
#[derive(Debug, Clone, Copy, Default)]
pub struct EndpointResponses<'a> {
    /// `/alpha/whoami` 的响应体。
    pub whoami: Option<Result<&'a Value, &'a str>>,
    /// `/alpha/billing/credits` 的响应体。
    pub credits: Option<Result<&'a Value, &'a str>>,
    /// `/alpha/billing/subscriptions` 的响应体。
    pub subscriptions: Option<Result<&'a Value, &'a str>>,
    /// `/alpha/usage/summary` 的响应体（当前只参与「至少一个端点可用」的判定）。
    pub usage_summary: Option<Result<&'a Value, &'a str>>,
}

/// 套餐 id → 月度信用额。未知套餐返回 None。
///
/// 匹配前先归一化：去空白、转小写、下划线换连字符（参考实现 `planInfo`）。
/// 采用**最长前缀优先**：`individual-pro-v1` 命中 80 而不是 `individual-pro` 的 30。
pub fn plan_monthly_cap(plan_id: &str) -> Option<f64> {
    // 空串必须判定为未知：否则会命中任意前缀，把「没有套餐」误报成有额度
    if plan_id.trim().is_empty() {
        return None;
    }
    let normalized = plan_id.trim().to_ascii_lowercase().replace('_', "-");
    KNOWN_PLANS
        .iter()
        .find(|(prefix, _, _)| normalized.starts_with(prefix))
        .map(|(_, _, monthly)| *monthly)
}

/// 已知套餐的展示名；未知返回 None。
pub fn plan_display_name(plan_id: &str) -> Option<&'static str> {
    if plan_id.trim().is_empty() {
        return None;
    }
    let normalized = plan_id.trim().to_ascii_lowercase().replace('_', "-");
    KNOWN_PLANS
        .iter()
        .find(|(prefix, _, _)| normalized.starts_with(prefix))
        .map(|(_, name, _)| *name)
}

/// 由余额、套餐与账期结束时间派生月度额度。
///
/// 只有在「套餐已知且账期结束时间可信」时才做减法（`derived = true`）；
/// 否则 `derived = false` 且把余额原样放进 `used` 供展示，见 [`MonthlyQuota`] 的说明。
fn derive_monthly(
    credits: &Credits,
    plan_id: Option<&str>,
    period_end_ms: Option<i64>,
) -> MonthlyQuota {
    let cap = plan_id.and_then(plan_monthly_cap);
    let reset_at_ms = period_end_ms.unwrap_or(0);
    // 周期结束时间为 0 或缺失时，派生值没有可信的重置时间 → 降级为只报余额
    let derived = cap.is_some() && reset_at_ms > 0;
    match cap {
        Some(cap) if derived => MonthlyQuota {
            // 赠送/购买额度可能让余额超过套餐额度，减法结果夹到 0，避免出现负的「已用」
            used: (cap - credits.monthly).max(0.0),
            cap,
            reset_at_ms,
            derived,
        },
        _ => MonthlyQuota {
            used: credits.monthly.max(0.0),
            cap: cap.unwrap_or(0.0),
            reset_at_ms,
            derived: false,
        },
    }
}

/// 在 windowLimits 里定位窗口对象：兼容 `fiveHour` / `five_hour` / `rolling5h` / `5h` 等别名。
///
/// `names` 的顺序即优先级；找不到时返回 None（该窗口缺失）。
fn window_object_present(window_limits: Option<&Value>, names: &[&str]) -> Option<Value> {
    let limits = window_limits?;
    names
        .iter()
        .find_map(|name| limits.get(*name).filter(|child| is_record(child)).cloned())
}

/// 窗口在响应里实际使用的键名；找不到返回 [`UNKNOWN_WINDOW`]。
fn find_window_key<'a>(window_limits: &Value, names: &'a [&'a str]) -> &'a str {
    names
        .iter()
        .copied()
        .find(|name| window_limits.get(*name).is_some_and(is_record))
        .unwrap_or(UNKNOWN_WINDOW)
}

/// 5 小时窗口的展示键名（找不到时回落到规范名 fiveHour）。
fn five_hour_key(window_limits: Option<&Value>) -> String {
    const NAMES: &[&str] = &["fiveHour", "five_hour", "rolling5h", "5h"];
    window_limits
        .map(|limits| find_window_key(limits, NAMES).to_string())
        .filter(|key| key != UNKNOWN_WINDOW)
        .unwrap_or_else(|| "fiveHour".to_string())
}

/// 周窗口的展示键名（找不到时回落到规范名 weekly）。
fn weekly_key(window_limits: Option<&Value>) -> String {
    const NAMES: &[&str] = &["weekly", "week"];
    window_limits
        .map(|limits| find_window_key(limits, NAMES).to_string())
        .filter(|key| key != UNKNOWN_WINDOW)
        .unwrap_or_else(|| "weekly".to_string())
}

/// 把四类端点响应组装成 [`QuotaSnapshot`]。
///
/// 处理顺序与参考实现 `fetchReport` 一致：whoami → credits → subscriptions →
/// usage/summary，每个端点独立失败并汇总进 `last_error`。`now_ms` 由调用方注入
/// （当前实现不消费它，保留是为了让「快照是否过期」这类判断将来能在本模块内被
/// 确定性单测覆盖）—— 见 docs/STYLE.md 第 2.3 节：库函数内部不读系统时间。
///
/// 上游 401/403 在真机上意味着「密钥无效」，调用方**不应**据此改动账号池状态：
/// docs/PROTOCOL.md 第 7 节明确要求「探针失败不得改变账号池状态」。
pub fn build_snapshot(responses: EndpointResponses<'_>, now_ms: i64) -> QuotaSnapshot {
    let _ = now_ms;
    let mut snapshot = QuotaSnapshot::default();
    let mut failures: Vec<String> = Vec::new();

    // 1) whoami —— 身份 + orgId（订阅端点要用它拼 ?orgId=）
    match responses.whoami {
        Some(Ok(body)) => snapshot.identity = AccountIdentity::from_whoami(body),
        Some(Err(reason)) => failures.push(format!("whoami: {reason}")),
        None => {}
    }

    // 2) billing/credits —— 余额 + 两个滚动窗口
    let mut credits_plan_id: Option<String> = None;
    match responses.credits {
        Some(Ok(body)) => {
            // 根上直接给，或包在 data 里（参考实现两条分支都试）
            let credits_value = get_record(body, "credits")
                .or_else(|| get_record(body, "data").and_then(|data| get_record(data, "credits")));
            let window_limits = get_record(body, "windowLimits").or_else(|| {
                get_record(body, "data").and_then(|data| get_record(data, "windowLimits"))
            });

            snapshot.credits = credits_value.and_then(Credits::from_json);
            credits_plan_id = snapshot.credits.as_ref().and_then(|c| c.plan_id.clone());

            if let Some(limits) = window_limits {
                if let Some(window) = window_object_present(
                    Some(limits),
                    &["fiveHour", "five_hour", "rolling5h", "5h"],
                ) {
                    snapshot.five_hour = WindowUsage::from_json(&window);
                }
                if let Some(window) = window_object_present(Some(limits), &["weekly", "week"]) {
                    snapshot.weekly = WindowUsage::from_json(&window);
                }
            }

            // 窗口超限告警：显式 exceeded 或 used >= cap 都算
            if snapshot.five_hour.as_ref().is_some_and(|w| w.exceeded) {
                snapshot
                    .alerts
                    .push(QuotaAlert::WindowExceeded(five_hour_key(window_limits)));
            }
            if snapshot.weekly.as_ref().is_some_and(|w| w.exceeded) {
                snapshot
                    .alerts
                    .push(QuotaAlert::WindowExceeded(weekly_key(window_limits)));
            }

            // 低余额告警：上游 belowThreshold 优先；缺失时用可用余额推算
            let explicit_below = body
                .get("belowThreshold")
                .or_else(|| get_record(body, "data").and_then(|d| d.get("belowThreshold")));
            let below = match explicit_below {
                Some(_) => bool_flag(explicit_below),
                None => snapshot
                    .credits
                    .as_ref()
                    .is_some_and(|c| c.monthly + c.purchased + c.free < LOW_BALANCE_THRESHOLD),
            };
            if below {
                snapshot.alerts.push(QuotaAlert::LowBalance);
            }
        }
        Some(Err(reason)) => failures.push(format!("billing/credits: {reason}")),
        None => {}
    }

    // 3) billing/subscriptions —— 套餐、状态、账期结束（失败时回落到 credits.planId）
    let mut period_end_ms: Option<i64> = None;
    match responses.subscriptions {
        Some(Ok(body)) => {
            let data = get_record(body, "data").or_else(|| get_record(body, "subscription"));
            snapshot.plan_id = data
                .and_then(|d| string_any(d, &["planId", "plan_id"]))
                .or_else(|| credits_plan_id.clone());
            snapshot.plan_status = data.and_then(|d| string_any(d, &["status"]));
            period_end_ms =
                data.and_then(|d| epoch_ms_any(d, &["currentPeriodEnd", "current_period_end"]));
            snapshot.period_end_ms = period_end_ms;

            if data.is_some_and(|d| bool_flag(d.get("cancelAtPeriodEnd"))) {
                snapshot.alerts.push(QuotaAlert::CancelsAtPeriodEnd);
            }
            // pendingPhase 的形态未公开：任何非 null 值都视为「有待生效变更」
            let pending = data
                .and_then(|d| d.get("pendingPhase"))
                .is_some_and(|phase| !phase.is_null());
            if pending {
                snapshot.alerts.push(QuotaAlert::PendingPlanChange);
            }
        }
        Some(Err(reason)) => {
            // 订阅失败时仍要用 credits 里的 planId 兜底，否则月度额度会莫名消失
            snapshot.plan_id = credits_plan_id.clone();
            failures.push(format!("billing/subscriptions: {reason}"));
        }
        None => snapshot.plan_id = credits_plan_id.clone(),
    }

    // 4) usage/summary —— 当前不参与派生，仅作为「还有一个端点可用」的信号
    if let Some(Err(reason)) = responses.usage_summary {
        failures.push(format!("usage/summary: {reason}"));
    }

    // 月度派生：cap 来自套餐映射，used = cap − 余额，重置时间 = 账期结束
    if let Some(credits) = snapshot.credits.as_ref() {
        snapshot.monthly = Some(derive_monthly(
            credits,
            snapshot.plan_id.as_deref(),
            period_end_ms,
        ));
    }

    // 部分失败与全部失败都记录原因（诊断日志用）；全部失败时前端据 last_error 显示「无法连接」
    if !failures.is_empty() {
        snapshot.last_error = Some(failures.join("; "));
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 快照构建的固定「现在」；库代码不读系统时间，测试也不需要真实时钟。
    const NOW_MS: i64 = 1_700_000_000_000;

    // ---- 套餐映射 ----

    #[test]
    fn longest_plan_prefix_wins_over_shorter_one() {
        // Pro v1（80）与 Pro（30）共用前缀，最长的必须优先，否则额度差 2.6 倍
        assert_eq!(plan_monthly_cap("individual-pro-v1"), Some(80.0));
        assert_eq!(plan_monthly_cap("individual-pro"), Some(30.0));
    }

    #[test]
    fn plan_ids_are_normalized_before_matching() {
        // 上游时而给下划线命名；归一化后应命中同一套餐
        assert_eq!(plan_monthly_cap("individual_pro"), Some(30.0));
        assert_eq!(plan_monthly_cap("  INDIVIDUAL-GO  "), Some(10.0));
    }

    #[test]
    fn unknown_and_empty_plans_have_no_cap() {
        assert_eq!(plan_monthly_cap(""), None, "空套餐不得命中任何前缀");
        assert_eq!(plan_monthly_cap("enterprise-secret"), None);
        assert_eq!(plan_display_name("enterprise-secret"), None);
        assert_eq!(plan_display_name("teams-pro"), Some("Teams Pro"));
    }

    // ---- resetAt 两种形式 ----

    #[test]
    fn reset_at_accepts_iso_string_and_epoch_milliseconds() {
        let from_iso = WindowUsage::from_json(&json!({
            "used": 1, "cap": 10, "resetAt": "2026-01-01T00:00:00Z"
        }))
        .expect("窗口对象应能解析");
        let from_ms = WindowUsage::from_json(&json!({
            "used": 1, "cap": 10, "resetAt": 1_767_225_600_000i64
        }))
        .expect("窗口对象应能解析");
        assert_eq!(
            from_iso.reset_at_ms, from_ms.reset_at_ms,
            "ISO 字符串与毫秒时间戳必须解析成同一个时刻"
        );
        assert_eq!(from_iso.reset_at_ms, 1_767_225_600_000);
    }

    #[test]
    fn reset_at_converts_second_level_timestamps() {
        // 10 位秒级时间戳（参考实现按 <1e12 判定）要与同刻的毫秒值一致
        let from_secs =
            WindowUsage::from_json(&json!({ "resetAt": 1_767_225_600i64 })).expect("应能解析");
        let from_ms =
            WindowUsage::from_json(&json!({ "resetAt": 1_767_225_600_000i64 })).expect("应能解析");
        assert_eq!(from_secs.reset_at_ms, from_ms.reset_at_ms);
    }

    #[test]
    fn reset_at_supports_offsets_and_fractional_seconds() {
        let parsed = WindowUsage::from_json(&json!({
            "resetAt": "2026-01-01T08:00:00.500+08:00"
        }))
        .expect("带时区偏移的 ISO 也应解析");
        assert_eq!(
            parsed.reset_at_ms, 1_767_225_600_500,
            "+08:00 的 08:00 与 UTC 00:00:00.500 是同一时刻"
        );
    }

    #[test]
    fn unparsable_reset_at_degrades_to_zero_instead_of_panicking() {
        let window =
            WindowUsage::from_json(&json!({ "resetAt": "不是时间" })).expect("仍应解析出窗口");
        assert_eq!(window.reset_at_ms, 0, "无法解析的重置时间降级为 0（未知）");
        let trailing_junk = WindowUsage::from_json(&json!({ "resetAt": 12.5 })).expect("应能解析");
        assert_eq!(
            trailing_junk.reset_at_ms, 12_500,
            "秒级小数时间戳按毫秒精度换算"
        );
    }

    #[test]
    fn reset_wait_uses_injected_now() {
        let window =
            WindowUsage::from_json(&json!({ "resetAt": 1_767_225_600_000i64 })).expect("应能解析");
        assert_eq!(
            window_reset_wait_ms(&window, 1_767_225_600_000i64 - 60_000),
            60_000,
            "等待时长必须由注入的 now_ms 决定"
        );
        assert_eq!(
            window_reset_wait_ms(&window, 1_767_225_600_000i64 + 1),
            0,
            "已过重置时间应返回 0 而不是负数"
        );
        let unknown = WindowUsage::from_json(&json!({})).expect("应能解析");
        assert_eq!(
            window_reset_wait_ms(&unknown, 0),
            0,
            "重置时间未知时不得编造等待"
        );
    }

    // ---- 数字类型 ----

    #[test]
    fn integers_and_floats_are_both_accepted() {
        let integers = Credits::from_json(&json!({
            "monthlyCredits": 30, "purchasedCredits": 5, "freeCredits": 0
        }))
        .expect("整数形式应能解析");
        let floats = Credits::from_json(&json!({
            "monthlyCredits": 30.0, "purchasedCredits": 5.5, "freeCredits": 0.25
        }))
        .expect("浮点形式应能解析");
        assert_eq!(integers.monthly, 30.0);
        assert_eq!(floats.monthly, 30.0, "整数与浮点必须归一成同一个数值");
        assert_eq!(floats.purchased, 5.5);
        assert_eq!(floats.free, 0.25);
    }

    // ---- 防御性解析 ----

    #[test]
    fn missing_fields_degrade_to_defaults() {
        let identity = AccountIdentity::from_whoami(&json!({}));
        assert_eq!(identity.label, "", "没有任何可展示字段时 label 为空串");
        assert_eq!(identity.user_id, None);

        let credits = Credits::from_json(&json!({})).expect("空对象仍是合法的 credits");
        assert_eq!(credits.monthly, 0.0);
        assert_eq!(credits.plan_id, None);
    }

    #[test]
    fn wrong_types_do_not_panic_and_do_not_fabricate_numbers() {
        // null / 字符串 / 数组都见过；一律降级，绝不能 panic 或当成 0 参与派生
        let credits = Credits::from_json(&json!({
            "monthlyCredits": null,
            "purchasedCredits": "12",
            "freeCredits": [1, 2],
            "planId": 42
        }))
        .expect("类型漂移时仍应返回对象");
        assert_eq!(credits.monthly, 0.0, "null 降级为 0");
        assert_eq!(credits.purchased, 0.0, "字符串数字不猜测，降级为 0");
        assert_eq!(credits.free, 0.0, "数组降级为 0");
        assert_eq!(
            credits.plan_id, None,
            "数字 planId 不是字符串标识，视为缺失"
        );
    }

    #[test]
    fn non_object_payloads_yield_none() {
        assert!(Credits::from_json(&Value::Null).is_none());
        assert!(Credits::from_json(&json!("nope")).is_none());
        assert!(WindowUsage::from_json(&json!([])).is_none());
        assert!(WindowUsage::from_json(&json!(3)).is_none());
    }

    #[test]
    fn label_falls_back_from_username_to_name_to_id() {
        let by_username = AccountIdentity::from_whoami(&json!({
            "user": { "id": "u1", "name": "张三", "userName": "zhangsan" }
        }));
        assert_eq!(by_username.label, "zhangsan", "userName 优先于 name");
        assert_eq!(by_username.user_id.as_deref(), Some("u1"));

        let by_name =
            AccountIdentity::from_whoami(&json!({ "user": { "id": "u1", "name": "张三" } }));
        assert_eq!(by_name.label, "张三", "userName 缺失时用 name");

        let by_id = AccountIdentity::from_whoami(&json!({ "user": { "id": "u1" } }));
        assert_eq!(by_id.label, "u1", "只有 id 时用 id 兜底");
    }

    #[test]
    fn whoami_reads_org_id_in_either_type() {
        let as_string = AccountIdentity::from_whoami(&json!({
            "user": { "id": "u1" }, "org": { "id": "org-1" }
        }));
        assert_eq!(as_string.org_id.as_deref(), Some("org-1"));
        let as_number =
            AccountIdentity::from_whoami(&json!({ "user": { "id": "u1" }, "org": { "id": 77 } }));
        assert_eq!(
            as_number.org_id.as_deref(),
            Some("77"),
            "数字 orgId 也要能拼进 URL"
        );
    }

    #[test]
    fn whoami_accepts_data_wrapper_and_username_alias() {
        let wrapped = AccountIdentity::from_whoami(&json!({
            "data": { "user": { "id": 5, "username": "fallback" } }
        }));
        assert_eq!(wrapped.label, "fallback", "兼容 data.user 与 username 拼写");
        assert_eq!(wrapped.user_id.as_deref(), Some("5"));
    }

    // ---- 告警 ----

    #[test]
    fn exceeded_window_produces_an_alert() {
        let body = json!({
            "credits": { "monthlyCredits": 25, "purchasedCredits": 10, "freeCredits": 5 },
            "windowLimits": {
                "fiveHour": { "used": 10, "cap": 10, "exceeded": true, "resetAt": NOW_MS + 3_600_000i64 },
                "weekly": { "used": 1, "cap": 100, "exceeded": false, "resetAt": NOW_MS + 86_400_000i64 }
            }
        });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(
            snapshot.alerts,
            vec![QuotaAlert::WindowExceeded("fiveHour".to_string())],
            "恰好用满的窗口应产生一条带窗口名的告警，未超限的窗口不产生"
        );
    }

    #[test]
    fn exceeded_window_is_inferred_when_flag_missing() {
        let body = json!({ "windowLimits": { "weekly": { "used": 100, "cap": 100 } } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(
            snapshot.alerts,
            vec![QuotaAlert::WindowExceeded("weekly".to_string())],
            "used 达到 cap 且无显式标记时也应判定为超限"
        );
    }

    #[test]
    fn window_below_cap_produces_no_alert() {
        // 边界：差一点点没到 cap，不得误报
        let body = json!({ "windowLimits": { "fiveHour": { "used": 99.9, "cap": 100 } } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert!(snapshot.alerts.is_empty(), "未达上限的窗口不得告警");
    }

    #[test]
    fn low_balance_fires_when_below_threshold() {
        let body =
            json!({ "credits": { "monthlyCredits": 1, "purchasedCredits": 0, "freeCredits": 0 } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(
            snapshot.alerts,
            vec![QuotaAlert::LowBalance],
            "余额低于阈值应告警"
        );
    }

    #[test]
    fn explicit_below_threshold_flag_wins_over_computed_balance() {
        // 上游给了明确标记时以它为准：这里余额看起来充足，但上游说低于阈值
        let body = json!({
            "credits": { "monthlyCredits": 100, "purchasedCredits": 0, "freeCredits": 0 },
            "belowThreshold": true
        });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(
            snapshot.alerts,
            vec![QuotaAlert::LowBalance],
            "显式标记优先于余额推算"
        );
    }

    #[test]
    fn healthy_account_produces_no_alerts() {
        let body = json!({
            "credits": { "monthlyCredits": 30, "purchasedCredits": 10, "freeCredits": 5 },
            "windowLimits": { "fiveHour": { "used": 1, "cap": 10 } }
        });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert!(snapshot.alerts.is_empty(), "额度充足时不应产生任何告警");
    }

    // ---- 月度派生 ----

    #[test]
    fn unknown_plan_degrades_monthly_to_balance_only() {
        let credits = json!({ "credits": { "monthlyCredits": 12.5 } });
        let subs = json!({ "data": { "planId": "enterprise-secret", "status": "active",
                                    "currentPeriodEnd": "2026-01-01T00:00:00Z" } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&credits)),
                subscriptions: Some(Ok(&subs)),
                ..Default::default()
            },
            NOW_MS,
        );
        let monthly = snapshot.monthly.expect("有余额就应产出月度视图");
        assert!(!monthly.derived, "未知套餐不得声称派生出可信的月度用量");
        assert_eq!(monthly.cap, 0.0, "未知套餐没有 cap");
        assert_eq!(monthly.used, 12.5, "降级时 used 承载剩余余额，供展示");
    }

    #[test]
    fn known_plan_derives_monthly_used_from_balance() {
        let credits = json!({ "credits": { "monthlyCredits": 22 } });
        let subs = json!({ "data": { "planId": "individual-pro", "status": "active",
                                    "currentPeriodEnd": 1_767_225_600_000i64 } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&credits)),
                subscriptions: Some(Ok(&subs)),
                ..Default::default()
            },
            NOW_MS,
        );
        let monthly = snapshot.monthly.expect("应产出月度视图");
        assert!(monthly.derived);
        assert_eq!(monthly.cap, 30.0);
        assert_eq!(monthly.used, 8.0, "used = cap(30) − 剩余(22)");
        assert_eq!(
            monthly.reset_at_ms, 1_767_225_600_000,
            "重置时间取计费周期结束"
        );
    }

    #[test]
    fn missing_period_end_degrades_to_balance_only() {
        // 账期结束时间未知时，派生值没有可信重置时间，必须降级
        let credits = json!({ "credits": { "monthlyCredits": 22 } });
        let subs = json!({ "data": { "planId": "individual-pro", "status": "active" } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&credits)),
                subscriptions: Some(Ok(&subs)),
                ..Default::default()
            },
            NOW_MS,
        );
        let monthly = snapshot.monthly.expect("应产出月度视图");
        assert!(!monthly.derived, "缺周期结束时间时不得声称派生成功");
        assert_eq!(monthly.cap, 30.0, "cap 仍可由套餐映射给出");
        assert_eq!(monthly.used, 22.0);
    }

    #[test]
    fn oversized_balance_never_yields_negative_usage() {
        // 赠送/购买额度可能让余额超过套餐额度，减法结果必须夹到 0
        let credits = json!({ "credits": { "monthlyCredits": 50 } });
        let subs = json!({ "data": { "planId": "individual-go",
                                    "currentPeriodEnd": 1_767_225_600_000i64 } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&credits)),
                subscriptions: Some(Ok(&subs)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(snapshot.monthly.expect("应产出月度视图").used, 0.0);
    }

    // ---- 端点独立性 ----

    #[test]
    fn failing_endpoint_does_not_block_others() {
        let credits = json!({
            "credits": { "monthlyCredits": 3 },
            "windowLimits": { "fiveHour": { "used": 5, "cap": 5, "exceeded": true } }
        });
        let snapshot = build_snapshot(
            EndpointResponses {
                whoami: Some(Err("HTTP 500")),
                credits: Some(Ok(&credits)),
                subscriptions: Some(Err("HTTP 503")),
                usage_summary: None,
            },
            NOW_MS,
        );
        assert_eq!(snapshot.credits.expect("credits 成功就应保留").monthly, 3.0);
        assert!(snapshot.five_hour.is_some(), "credits 成功时窗口必须保留");
        assert!(
            snapshot
                .alerts
                .contains(&QuotaAlert::WindowExceeded("fiveHour".to_string())),
            "部分端点失败不影响窗口告警"
        );
        let last_error = snapshot.last_error.expect("部分失败也要记录诊断信息");
        assert!(
            last_error.contains("whoami: HTTP 500"),
            "失败原因应可定位到端点：{last_error}"
        );
        assert!(last_error.contains("billing/subscriptions: HTTP 503"));
    }

    #[test]
    fn subscription_plan_id_falls_back_to_credits() {
        // 订阅端点挂了，但 credits 里带了 planId：月度派生仍应成立
        let credits = json!({ "credits": { "monthlyCredits": 20, "planId": "individual-max" } });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&credits)),
                subscriptions: Some(Err("HTTP 502")),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(snapshot.plan_id.as_deref(), Some("individual-max"));
        // 订阅失败 → 账期结束未知 → 降级，但 cap 仍来自映射
        let monthly = snapshot.monthly.expect("应产出月度视图");
        assert!(!monthly.derived);
        assert_eq!(monthly.cap, 150.0);
    }

    #[test]
    fn all_endpoints_failing_reports_overall_error() {
        let snapshot = build_snapshot(
            EndpointResponses {
                whoami: Some(Err("HTTP 401")),
                credits: Some(Err("HTTP 401")),
                subscriptions: Some(Err("HTTP 401")),
                usage_summary: Some(Err("HTTP 401")),
            },
            NOW_MS,
        );
        assert!(snapshot.credits.is_none());
        assert!(snapshot.monthly.is_none());
        let last_error = snapshot.last_error.expect("全部失败必须有错误信息");
        assert!(
            last_error.contains("whoami: HTTP 401")
                && last_error.contains("usage/summary: HTTP 401")
        );
        assert!(snapshot.alerts.is_empty(), "没有数据时不得凭空产生告警");
    }

    #[test]
    fn empty_bodies_degrade_without_error_string() {
        // 端点返回 200 但是空对象：算「成功但无数据」，不应报连接失败
        let empty = json!({});
        let snapshot = build_snapshot(
            EndpointResponses {
                whoami: Some(Ok(&empty)),
                credits: Some(Ok(&empty)),
                subscriptions: Some(Ok(&empty)),
                usage_summary: Some(Ok(&empty)),
            },
            NOW_MS,
        );
        assert!(snapshot.last_error.is_none(), "空响应不是失败");
        assert!(
            snapshot.monthly.is_none(),
            "没有 credits 就不应产出月度视图"
        );
        assert!(snapshot.plan_id.is_none());
    }

    #[test]
    fn cancellation_and_pending_phase_produce_subscription_alerts() {
        let subs = json!({
            "data": { "planId": "individual-go", "status": "active",
                      "currentPeriodEnd": 1_767_225_600_000i64,
                      "cancelAtPeriodEnd": true,
                      "pendingPhase": { "planId": "individual-pro" } }
        });
        let snapshot = build_snapshot(
            EndpointResponses {
                subscriptions: Some(Ok(&subs)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(snapshot.plan_status.as_deref(), Some("active"));
        assert!(
            snapshot.alerts.contains(&QuotaAlert::CancelsAtPeriodEnd),
            "标记不续费应产生告警"
        );
        assert!(
            snapshot.alerts.contains(&QuotaAlert::PendingPlanChange),
            "存在待生效套餐变更应产生告警"
        );
    }

    #[test]
    fn window_aliases_are_resolved_and_named_in_alerts() {
        // 字段漂移成 five_hour / week 时仍要能识别，告警里报的应是实际键名
        let body = json!({
            "windowLimits": {
                "five_hour": { "usage": 3, "limit": 3, "exceeded": "true" },
                "week": { "usedCredits": 1, "capCredits": 10 }
            }
        });
        let snapshot = build_snapshot(
            EndpointResponses {
                credits: Some(Ok(&body)),
                ..Default::default()
            },
            NOW_MS,
        );
        assert_eq!(snapshot.five_hour.expect("应识别 five_hour 别名").used, 3.0);
        assert_eq!(snapshot.weekly.expect("应识别 week 别名").cap, 10.0);
        assert_eq!(
            snapshot.alerts,
            vec![QuotaAlert::WindowExceeded("five_hour".to_string())],
            "字符串 \"true\" 也算超限，告警名用响应里的实际键名"
        );
    }

    #[test]
    fn alert_kind_and_message_are_stable_for_ui() {
        assert_eq!(QuotaAlert::LowBalance.kind(), "low_balance");
        assert_eq!(
            QuotaAlert::WindowExceeded("fiveHour".to_string()).kind(),
            "window_exceeded"
        );
        assert!(
            QuotaAlert::LowBalance.message().contains("剩余额度"),
            "面向用户的文案应说明是什么问题"
        );
    }

    #[test]
    fn snapshot_survives_unknown_shapes_end_to_end() {
        // 一次性喂进各种畸形输入，确保整条链路不 panic（这是面板白屏的根因）
        let whoami = json!([1, 2, 3]);
        let credits = json!({ "credits": "oops", "windowLimits": 7 });
        let subs = json!({ "data": { "planId": true, "currentPeriodEnd": [] } });
        let summary = json!(null);
        let snapshot = build_snapshot(
            EndpointResponses {
                whoami: Some(Ok(&whoami)),
                credits: Some(Ok(&credits)),
                subscriptions: Some(Ok(&subs)),
                usage_summary: Some(Ok(&summary)),
            },
            NOW_MS,
        );
        assert!(snapshot.credits.is_none(), "畸形 credits 应降级为 None");
        assert!(snapshot.monthly.is_none());
        assert_eq!(snapshot.identity.label, "", "畸形 whoami 降级为空身份");
        assert_eq!(snapshot.plan_id, None);
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let snapshot = QuotaSnapshot {
            identity: AccountIdentity {
                user_id: Some("u-1".into()),
                user_name: Some("user1".into()),
                label: "User One".into(),
                org_id: Some("org-1".into()),
            },
            credits: Some(Credits {
                monthly: 10.0,
                purchased: 5.0,
                free: 0.0,
                plan_id: Some("individual-pro".into()),
            }),
            five_hour: Some(WindowUsage {
                used: 2.0,
                cap: 10.0,
                exceeded: false,
                reset_at_ms: 1000,
            }),
            weekly: None,
            monthly: Some(MonthlyQuota {
                used: 20.0,
                cap: 30.0,
                reset_at_ms: 2000,
                derived: true,
            }),
            plan_id: Some("individual-pro".into()),
            plan_status: Some("active".into()),
            period_end_ms: Some(2000),
            alerts: vec![
                QuotaAlert::LowBalance,
                QuotaAlert::WindowExceeded("fiveHour".into()),
            ],
            last_error: None,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: QuotaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snapshot);
    }
}
