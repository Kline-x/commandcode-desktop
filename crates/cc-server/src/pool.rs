//! 账号池的选择与轮换状态（纯逻辑，无 I/O）。
//!
//! 移植自 Mars-Sea/dsh-commandcode-provider 的 src/accounts.ts
//! （MIT，commit 7cf3235，本地副本见 third_party/dsh-accounts.ts），并保持其语义：
//!
//! - 轮换状态**以 API key 为键**，因此两个 slot 解析出同一个 key 时共享状态；
//! - Unknown（429，重置时间未知）与 Disabled（401，直到凭据变更）都不可用；
//! - Cooldown 在 until 到达后自行恢复可用；
//! - 选择顺序：**模型路由规则 → 手动指定的账号 → 轮转顺序中第一个可用账号**；
//! - 路由规则是**提示而非硬门禁**：命中的账号不可用时回落到常规选择。
//!
//! 所有时间都通过参数注入（now_ms），不在函数内部读取系统时间——这样
//! 过期行为可以被确定性地单测覆盖。

use crate::error::CcError;
use crate::sse::Usage;

/// 单次请求允许的最大账号轮换次数。
///
/// 与上游一致：即使某个钩子行为异常，也不会在一个请求内无限轮换。
pub const MAX_ACCOUNT_ROTATIONS: usize = 16;

/// 全池耗尽时，可附加给客户端的重试等待上限（毫秒）。
///
/// 必须与适配器侧的重试策略 backoff.max_delay_ms 保持一致：超过该上限的
/// 「附带等待」会让重试执行器直接放弃重试，而不是退回本地退避——那会把
/// 「等到窗口重置」变成「立刻失败」。
pub const RETRY_MAX_DELAY_MS: u64 = 900_000;

/// 一个账号槽位（配置层的事实）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSlot {
    /// 稳定 id：默认账号为 default，其余为其凭据引用或 account-N。
    pub id: String,
    /// 展示名。
    pub label: String,
}

/// 一个 key 为何停止服务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    /// 429：被限流；窗口重置时间未知，需探测。
    RateLimit,
    /// 401：凭据无效；直到存储的凭据变更前一直跳过。
    InvalidCredential,
}

/// 轮换状态的形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStateKind {
    /// 429 标记，窗口重置时间未知直到探测完成。
    Unknown,
    /// 已探测（或已知重置时间）：在 until_ms 之前不可用。
    Cooldown { until_ms: i64 },
    /// 401 标记：在凭据变更前一直跳过。
    Disabled,
}

/// 一个 key 的轮换状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub kind: AccountStateKind,
    /// 人类可读的标记原因（例如 rate limited (429)）。
    pub reason: String,
}

impl AccountState {
    /// 由一次拒绝构造标记。
    pub fn rejected(kind: RejectionKind) -> Self {
        match kind {
            RejectionKind::RateLimit => Self {
                kind: AccountStateKind::Unknown,
                reason: "rate limited (429)".to_string(),
            },
            RejectionKind::InvalidCredential => Self {
                kind: AccountStateKind::Disabled,
                reason: "invalid API key (401)".to_string(),
            },
        }
    }

    /// 由一次窗口探测构造标记。
    ///
    /// 返回值的三态语义必须区分清楚——这是调用方最容易写错的地方：
    /// - `Some(Some(state))`：窗口仍超限，记录 cooldown；
    /// - `Some(None)`：窗口已恢复，**明确清除**标记；
    /// - `None`：探测失败，**无新信息**，调用方必须保留旧状态。
    ///
    /// 把「恢复」与「探测失败」压成同一个 None 会让失败的探测把账号错误地复活。
    pub fn after_probe(
        previous: Option<&AccountState>,
        probe: Option<WindowProbe>,
    ) -> Option<Option<Self>> {
        let probe = probe?;
        if !probe.exceeded {
            return Some(None);
        }
        Some(Some(Self {
            kind: AccountStateKind::Cooldown {
                until_ms: probe.reset_at_ms,
            },
            reason: previous
                .map(|p| p.reason.clone())
                .unwrap_or_else(|| "rate limited (429)".to_string()),
        }))
    }
}

/// 槽位与其已解析出的 key 配对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccount {
    pub slot: AccountSlot,
    /// 已 trim 归一化的 key；轮换状态以此为键。
    pub key: String,
    /// None 表示从未被拒绝，可用。
    pub state: Option<AccountState>,
}

/// 该账号此刻是否可服务。
pub fn account_usable(state: Option<&AccountState>, now_ms: i64) -> bool {
    match state {
        None => true,
        Some(s) => match s.kind {
            AccountStateKind::Cooldown { until_ms } => until_ms > 0 && now_ms >= until_ms,
            // Unknown（429，重置未知）与 Disabled（401）都不可用
            AccountStateKind::Unknown | AccountStateKind::Disabled => false,
        },
    }
}

/// 选出此刻应当服务的账号：手动指定的账号可用时优先，否则轮转顺序里第一个可用者。
///
/// 池（请求路径）与用量视图（「活跃账号」徽标）共用本函数，保证两者永远一致。
pub fn select_active_account<'a>(
    accounts: &'a [ResolvedAccount],
    preferred_id: Option<&str>,
    now_ms: i64,
) -> Option<&'a ResolvedAccount> {
    let usable = || {
        accounts
            .iter()
            .filter(|a| account_usable(a.state.as_ref(), now_ms))
    };
    if let Some(preferred) = preferred_id {
        if let Some(hit) = usable().find(|a| a.slot.id == preferred) {
            return Some(hit);
        }
    }
    usable().next()
}

/// 一条「把这些模型固定路由到那个账号」的规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAccountRule {
    /// 目录中的模型 id。
    pub models: Vec<String>,
    /// 目标账号的槽位 id。
    pub account: String,
}

/// 第一条命中该模型的路由规则（**按列表顺序，首个命中生效**）。
pub fn match_model_rule<'a>(
    model: &str,
    rules: &'a [ModelAccountRule],
) -> Option<&'a ModelAccountRule> {
    if model.is_empty() {
        return None;
    }
    rules.iter().find(|r| r.models.iter().any(|m| m == model))
}

/// 该模型应路由到的账号：规则命中且该账号可用时返回它。
///
/// 未命中或目标账号不可用时返回 None——调用方随即回落到常规选择。
/// 路由是**提示，不是硬门禁**。
pub fn select_account_for_model<'a>(
    accounts: &'a [ResolvedAccount],
    model: &str,
    rules: &[ModelAccountRule],
    now_ms: i64,
) -> Option<&'a ResolvedAccount> {
    let rule = match_model_rule(model, rules)?;
    accounts
        .iter()
        .find(|a| a.slot.id == rule.account && account_usable(a.state.as_ref(), now_ms))
}

/// 一次五小时窗口探测的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowProbe {
    /// 该窗口是否已超限。
    pub exceeded: bool,
    /// 窗口重置时刻（epoch 毫秒）。
    pub reset_at_ms: i64,
}

/// 一次请求的轮换状态机。
///
/// 这是**有状态**的部分：它记住哪些 key 已经试过、以及每个 key 为何被拒绝。
/// 与纯函数分开，是因为「每个 key 只试一次」这条不变式需要一个跨尝试的对象来持有。
#[derive(Debug)]
pub struct Rotation {
    /// 已经尝试过的 key（同一请求内不重复尝试）。
    tried: Vec<String>,
    /// 当前使用的 key。
    current: String,
}

/// 一次轮换尝试的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationStep {
    /// 换到下一个账号，继续重试。
    Switched {
        /// 新的 key。
        key: String,
        /// 新的槽位 id（用于日志与用量归属）。
        slot_id: String,
    },
    /// 没有更多账号可试，结束。
    Exhausted,
    /// 该错误与账号无关（换号无用），结束并原样抛出。
    NotRotatable,
}

impl Rotation {
    /// 开始一次请求的轮换：以给定 key 为起点。
    pub fn start(key: impl Into<String>) -> Self {
        let key = key.into();
        Self {
            tried: vec![key.clone()],
            current: key,
        }
    }

    /// 当前使用的 key。
    pub fn current_key(&self) -> &str {
        &self.current
    }

    /// 已尝试的 key 数量。
    pub fn attempted(&self) -> usize {
        self.tried.len()
    }

    /// 该 key 是否已经尝试过。
    pub fn has_tried(&self, key: &str) -> bool {
        self.tried.iter().any(|k| k == key)
    }

    /// 报告一次失败，决定下一步。
    ///
    /// \`error\` 决定「换号有没有用」（见 [CcError::rotates_account]）；
    /// \`next\` 是候选的下一个账号（由调用方通过池解析得出）。
    ///
    /// 返回 [RotationStep::Switched] 时内部状态已推进，调用方应当用新 key 重试。
    pub fn on_failure(&mut self, error: &CcError, next: Option<ResolvedAccount>) -> RotationStep {
        if !error.rotates_account() {
            return RotationStep::NotRotatable;
        }
        if self.tried.len() >= MAX_ACCOUNT_ROTATIONS {
            return RotationStep::Exhausted;
        }
        let Some(next) = next else {
            return RotationStep::Exhausted;
        };
        // 同一个 key 不重复尝试（池里两个 slot 可能解析出同一个 key）
        if self.has_tried(&next.key) {
            return RotationStep::Exhausted;
        }
        self.tried.push(next.key.clone());
        self.current = next.key.clone();
        RotationStep::Switched {
            key: next.key,
            slot_id: next.slot.id,
        }
    }
}

/// 账号池：持有轮换状态并提供选择。
///
/// 状态**按 key 存储**（不是按槽位）：两个槽位共用同一个 key 时共享一份状态，
/// 凭据变更后新 key 自动获得干净状态。
#[derive(Debug, Default)]
pub struct AccountPool {
    /// key → 状态。
    states: std::collections::HashMap<String, AccountState>,
}

impl AccountPool {
    /// 空池。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次拒绝。
    pub fn mark_rejected(&mut self, key: &str, kind: RejectionKind) {
        self.states
            .insert(key.to_string(), AccountState::rejected(kind));
    }

    /// 记录一次窗口探测结果。
    ///
    /// \`probe\` 为 None 表示探测本身失败——**不得**改变状态（失败的探测不携带信息）。
    pub fn apply_probe(&mut self, key: &str, probe: Option<WindowProbe>) {
        let previous = self.states.get(key).cloned();
        match AccountState::after_probe(previous.as_ref(), probe) {
            Some(Some(next)) => {
                self.states.insert(key.to_string(), next);
            }
            Some(None) => {
                self.states.remove(key);
            }
            None => {}
        }
    }

    /// 查询某 key 的状态。
    pub fn state(&self, key: &str) -> Option<&AccountState> {
        self.states.get(key)
    }

    /// 把槽位与 key 配对，并附上当前状态。
    ///
    /// \`keys\` 与 \`slots\` 一一对应；key 为 None 的槽位被跳过（未配置凭据）。
    pub fn resolve(&self, slots: &[AccountSlot], keys: &[Option<String>]) -> Vec<ResolvedAccount> {
        slots
            .iter()
            .zip(keys.iter())
            .filter_map(|(slot, key)| {
                let key = key.as_ref()?;
                Some(ResolvedAccount {
                    slot: slot.clone(),
                    key: key.clone(),
                    state: self.states.get(key).cloned(),
                })
            })
            .collect()
    }

    /// 选择本次请求要用的账号。
    ///
    /// 顺序：模型路由 → 手动指定 → 轮转顺序首个可用。
    pub fn select<'a>(
        &self,
        accounts: &'a [ResolvedAccount],
        model: &str,
        rules: &[ModelAccountRule],
        preferred_id: Option<&str>,
        now_ms: i64,
    ) -> Option<&'a ResolvedAccount> {
        if let Some(routed) = select_account_for_model(accounts, model, rules, now_ms) {
            return Some(routed);
        }
        select_active_account(accounts, preferred_id, now_ms)
    }

    /// 所有账号都不可用时的错误：带上**最早**的窗口重置时间。
    ///
    /// 若全部是 401 禁用，返回 [CcError::InvalidCredential]（这是配置问题，重试无用）。
    pub fn exhausted_error(&self, accounts: &[ResolvedAccount], now_ms: i64) -> CcError {
        if accounts.is_empty() {
            return CcError::MissingCredential;
        }
        let all_disabled = accounts.iter().all(|a| {
            matches!(
                a.state.as_ref().map(|s| s.kind),
                Some(AccountStateKind::Disabled)
            )
        });
        if all_disabled {
            return CcError::InvalidCredential;
        }
        let earliest = accounts
            .iter()
            .filter_map(|a| match a.state.as_ref().map(|s| s.kind) {
                Some(AccountStateKind::Cooldown { until_ms }) if until_ms > 0 => Some(until_ms),
                _ => None,
            })
            .min();
        CcError::all_accounts_exhausted(accounts.len(), earliest, now_ms)
    }
}

/// 一次生成的用量合计（跨轮换累加）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    /// 输入 token 合计。
    pub input_tokens: u64,
    /// 输出 token 合计。
    pub output_tokens: u64,
    /// 缓存命中合计。
    pub cached_input_tokens: u64,
}

impl UsageTotals {
    /// 累加一次生成的用量。
    pub fn add(&mut self, usage: Usage) {
        let usage = usage.normalized();
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cached_input_tokens += usage.cached_input_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(id: &str) -> AccountSlot {
        AccountSlot {
            id: id.to_string(),
            label: id.to_string(),
        }
    }

    fn account(id: &str, state: Option<AccountState>) -> ResolvedAccount {
        ResolvedAccount {
            slot: slot(id),
            key: format!("key-{id}"),
            state,
        }
    }

    const T0: i64 = 1_000_000;

    #[test]
    fn never_rejected_is_usable() {
        assert!(account_usable(None, T0));
    }

    #[test]
    fn cooldown_expires_at_its_reset_time() {
        let state = AccountState {
            kind: AccountStateKind::Cooldown { until_ms: T0 },
            reason: "rate limited (429)".into(),
        };
        assert!(
            !account_usable(Some(&state), T0 - 1),
            "重置前一毫秒仍不可用"
        );
        assert!(account_usable(Some(&state), T0), "到达重置时刻即可用");
    }

    #[test]
    fn cooldown_without_known_reset_is_not_usable() {
        let state = AccountState {
            kind: AccountStateKind::Cooldown { until_ms: 0 },
            reason: "rate limited (429)".into(),
        };
        assert!(!account_usable(Some(&state), T0));
    }

    #[test]
    fn unknown_and_disabled_are_never_usable() {
        let unknown = AccountState::rejected(RejectionKind::RateLimit);
        let disabled = AccountState::rejected(RejectionKind::InvalidCredential);
        assert_eq!(unknown.kind, AccountStateKind::Unknown);
        assert_eq!(disabled.kind, AccountStateKind::Disabled);
        assert!(!account_usable(Some(&unknown), i64::MAX));
        assert!(!account_usable(Some(&disabled), i64::MAX));
    }

    #[test]
    fn preferred_account_wins_when_usable() {
        let accounts = vec![account("default", None), account("second", None)];
        let picked = select_active_account(&accounts, Some("second"), T0).unwrap();
        assert_eq!(picked.slot.id, "second");
    }

    #[test]
    fn exhausted_preferred_falls_back_to_rotation_order() {
        let accounts = vec![
            account("default", None),
            account(
                "second",
                Some(AccountState {
                    kind: AccountStateKind::Cooldown {
                        until_ms: T0 + 60_000,
                    },
                    reason: "rate limited (429)".into(),
                }),
            ),
        ];
        let picked = select_active_account(&accounts, Some("second"), T0).unwrap();
        assert_eq!(
            picked.slot.id, "default",
            "首选账号被限流时应回落到轮转顺序"
        );
    }

    #[test]
    fn unknown_preferred_id_is_ignored() {
        let accounts = vec![account("default", None)];
        let picked = select_active_account(&accounts, Some("nope"), T0).unwrap();
        assert_eq!(picked.slot.id, "default");
    }

    #[test]
    fn all_unusable_yields_none() {
        let state = AccountState::rejected(RejectionKind::RateLimit);
        let accounts = vec![account("a", Some(state.clone())), account("b", Some(state))];
        assert!(select_active_account(&accounts, None, T0).is_none());
    }

    fn rules() -> Vec<ModelAccountRule> {
        vec![
            ModelAccountRule {
                models: vec!["deepseek/deepseek-v4-pro".into()],
                account: "second".into(),
            },
            ModelAccountRule {
                models: vec![
                    "deepseek/deepseek-v4-pro".into(),
                    "tencent/hy4-preview".into(),
                ],
                account: "third".into(),
            },
        ]
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = rules();
        let hit = match_model_rule("deepseek/deepseek-v4-pro", &rules).unwrap();
        assert_eq!(hit.account, "second", "规则按列表顺序，首个命中生效");
    }

    #[test]
    fn empty_model_never_matches() {
        assert!(match_model_rule("", &rules()).is_none());
    }

    #[test]
    fn routed_account_serves_when_usable() {
        let accounts = vec![account("default", None), account("second", None)];
        let picked =
            select_account_for_model(&accounts, "deepseek/deepseek-v4-pro", &rules(), T0).unwrap();
        assert_eq!(picked.slot.id, "second");
    }

    #[test]
    fn unusable_routed_account_falls_back() {
        let accounts = vec![
            account(
                "second",
                Some(AccountState {
                    kind: AccountStateKind::Disabled,
                    reason: "invalid API key (401)".into(),
                }),
            ),
            account("third", None),
        ];
        // 命中第一条规则但目标不可用 → 返回 None，由调用方回落到常规选择。
        // 注意：不得自动顺延到下一条规则（与上游语义一致）。
        assert!(
            select_account_for_model(&accounts, "deepseek/deepseek-v4-pro", &rules(), T0).is_none()
        );
        assert_eq!(
            select_active_account(&accounts, None, T0).unwrap().slot.id,
            "third"
        );
    }

    #[test]
    fn probe_clears_state_when_window_reopened() {
        let previous = AccountState::rejected(RejectionKind::RateLimit);
        // Some(None) = 窗口已恢复，明确要求清除标记（区别于 None = 探测失败）
        assert_eq!(
            AccountState::after_probe(
                Some(&previous),
                Some(WindowProbe {
                    exceeded: false,
                    reset_at_ms: T0
                })
            ),
            Some(None)
        );
    }

    #[test]
    fn probe_records_cooldown_with_reset_time() {
        let previous = AccountState::rejected(RejectionKind::RateLimit);
        let next = AccountState::after_probe(
            Some(&previous),
            Some(WindowProbe {
                exceeded: true,
                reset_at_ms: T0 + 3_600_000,
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            next.kind,
            AccountStateKind::Cooldown {
                until_ms: T0 + 3_600_000
            }
        );
        assert_eq!(next.reason, "rate limited (429)", "沿用既有原因");
    }

    #[test]
    fn failed_probe_yields_no_information() {
        let previous = AccountState::rejected(RejectionKind::RateLimit);
        // None = 探测失败 = 无新信息，调用方据此**保留**旧状态（不是清除）
        assert_eq!(AccountState::after_probe(Some(&previous), None), None);
    }

    #[test]
    fn pool_shares_state_between_slots_with_same_key() {
        let mut pool = AccountPool::new();
        pool.mark_rejected("shared", RejectionKind::RateLimit);
        let slots = vec![slot("a"), slot("b")];
        let keys = vec![Some("shared".to_string()), Some("shared".to_string())];
        let resolved = pool.resolve(&slots, &keys);
        assert_eq!(resolved.len(), 2);
        assert!(
            resolved.iter().all(|a| a.state.is_some()),
            "共用 key 的槽位共享一份状态"
        );
    }

    #[test]
    fn pool_skips_slots_without_keys() {
        let pool = AccountPool::new();
        let slots = vec![slot("a"), slot("b")];
        let keys = vec![Some("k".to_string()), None];
        let resolved = pool.resolve(&slots, &keys);
        assert_eq!(resolved.len(), 1, "未配置凭据的槽位应被跳过");
        assert_eq!(resolved[0].slot.id, "a");
    }

    #[test]
    fn pool_apply_probe_removes_state_on_recovery() {
        let mut pool = AccountPool::new();
        pool.mark_rejected("k", RejectionKind::RateLimit);
        assert!(pool.state("k").is_some());
        pool.apply_probe(
            "k",
            Some(WindowProbe {
                exceeded: false,
                reset_at_ms: T0,
            }),
        );
        assert!(pool.state("k").is_none(), "窗口恢复后状态应被清除");
    }

    #[test]
    fn pool_failed_probe_keeps_state() {
        let mut pool = AccountPool::new();
        pool.mark_rejected("k", RejectionKind::RateLimit);
        pool.apply_probe("k", None);
        assert!(pool.state("k").is_some(), "探测失败不得改变池状态");
    }

    #[test]
    fn all_disabled_reports_invalid_credential_not_rate_limit() {
        let pool = AccountPool::new();
        let accounts = vec![
            account(
                "a",
                Some(AccountState::rejected(RejectionKind::InvalidCredential)),
            ),
            account(
                "b",
                Some(AccountState::rejected(RejectionKind::InvalidCredential)),
            ),
        ];
        assert_eq!(
            pool.exhausted_error(&accounts, T0),
            CcError::InvalidCredential
        );
    }

    #[test]
    fn exhausted_error_names_earliest_reset() {
        let pool = AccountPool::new();
        let accounts = vec![
            account(
                "a",
                Some(AccountState {
                    kind: AccountStateKind::Cooldown {
                        until_ms: T0 + 120_000,
                    },
                    reason: "rate limited (429)".into(),
                }),
            ),
            account(
                "b",
                Some(AccountState {
                    kind: AccountStateKind::Cooldown {
                        until_ms: T0 + 60_000,
                    },
                    reason: "rate limited (429)".into(),
                }),
            ),
        ];
        let err = pool.exhausted_error(&accounts, T0);
        assert_eq!(err.retry_after_ms(), Some(60_000), "应报告最早的重置时间");
        assert!(err.to_string().contains("2 个"), "错误信息应说明账号数量");
    }

    #[test]
    fn empty_pool_reports_missing_credential() {
        let pool = AccountPool::new();
        assert_eq!(pool.exhausted_error(&[], T0), CcError::MissingCredential);
    }

    #[test]
    fn rotation_switches_to_next_account_on_rate_limit() {
        let mut rot = Rotation::start("k1");
        let step = rot.on_failure(
            &CcError::UpstreamHttp {
                status: 429,
                code: None,
                body: String::new(),
            },
            Some(account("b", None)),
        );
        assert_eq!(
            step,
            RotationStep::Switched {
                key: "key-b".to_string(),
                slot_id: "b".to_string()
            }
        );
        assert_eq!(rot.current_key(), "key-b");
        assert_eq!(rot.attempted(), 2);
    }

    #[test]
    fn rotation_stops_on_errors_that_are_not_account_related() {
        let mut rot = Rotation::start("k1");
        // 403 套餐错误：换号无用，必须原样抛出而不是继续轮换
        let step = rot.on_failure(
            &CcError::UpstreamHttp {
                status: 403,
                code: Some("MODEL_NOT_IN_PLAN".into()),
                body: String::new(),
            },
            Some(account("b", None)),
        );
        assert_eq!(step, RotationStep::NotRotatable);
        assert_eq!(rot.current_key(), "k1", "不可轮换时不应改变当前 key");
    }

    #[test]
    fn rotation_never_reuses_a_key() {
        let mut rot = Rotation::start("k1");
        // 池解析出同一个 key：必须停下，否则会无限打同一个账号
        let step = rot.on_failure(
            &CcError::InvalidCredential,
            Some(ResolvedAccount {
                slot: slot("alias"),
                key: "k1".to_string(),
                state: None,
            }),
        );
        assert_eq!(step, RotationStep::Exhausted);
    }

    #[test]
    fn rotation_is_bounded_by_max_rotations() {
        // 起点 key 不能与候选 key 重合，否则会触发「不重复尝试」而提前结束
        let mut rot = Rotation::start("start");
        for i in 0..MAX_ACCOUNT_ROTATIONS - 1 {
            let next = ResolvedAccount {
                slot: slot(&format!("s{i}")),
                key: format!("k{i}"),
                state: None,
            };
            assert!(matches!(
                rot.on_failure(&CcError::InvalidCredential, Some(next)),
                RotationStep::Switched { .. }
            ));
        }
        // 到达上限后必须停止，即使还有候选可用
        let step = rot.on_failure(
            &CcError::InvalidCredential,
            Some(ResolvedAccount {
                slot: slot("extra"),
                key: "extra".into(),
                state: None,
            }),
        );
        assert_eq!(step, RotationStep::Exhausted);
        assert_eq!(rot.attempted(), MAX_ACCOUNT_ROTATIONS);
    }

    #[test]
    fn rotation_exhausts_when_no_candidate_remains() {
        let mut rot = Rotation::start("k1");
        assert_eq!(
            rot.on_failure(&CcError::InvalidCredential, None),
            RotationStep::Exhausted
        );
    }

    #[test]
    fn usage_totals_normalize_each_addition() {
        let mut totals = UsageTotals::default();
        // output=0 的那次应被整体清零（防伪账）
        totals.add(Usage {
            input_tokens: 100,
            output_tokens: 0,
            cached_input_tokens: 50,
        });
        assert_eq!(totals, UsageTotals::default());
        totals.add(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 2,
        });
        assert_eq!(totals.input_tokens, 10);
        assert_eq!(totals.output_tokens, 5);
        assert_eq!(totals.cached_input_tokens, 2);
    }
}
