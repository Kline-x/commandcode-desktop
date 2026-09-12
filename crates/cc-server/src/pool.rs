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

    /// 由一次窗口探测构造标记：窗口未超限则**清除**标记（返回 None）。
    ///
    /// 探测失败（probe 为 None）必须原样保留既有状态——失败的探测永远不得
    /// 改变池状态。返回 None 表示「无新信息」，由调用方保留旧状态。
    pub fn after_probe(
        previous: Option<&AccountState>,
        probe: Option<(bool, i64)>,
    ) -> Option<Self> {
        let (exceeded, reset_at) = probe?;
        if !exceeded {
            return None;
        }
        Some(Self {
            kind: AccountStateKind::Cooldown { until_ms: reset_at },
            reason: previous
                .map(|p| p.reason.clone())
                .unwrap_or_else(|| "rate limited (429)".to_string()),
        })
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
        assert!(AccountState::after_probe(Some(&previous), Some((false, T0))).is_none());
    }

    #[test]
    fn probe_records_cooldown_with_reset_time() {
        let previous = AccountState::rejected(RejectionKind::RateLimit);
        let next =
            AccountState::after_probe(Some(&previous), Some((true, T0 + 3_600_000))).unwrap();
        assert_eq!(
            next.kind,
            AccountStateKind::Cooldown {
                until_ms: T0 + 3_600_000
            }
        );
        assert_eq!(next.reason, "rate limited (429)", "沿用既有原因");
    }

    #[test]
    fn failed_probe_never_changes_state() {
        let previous = AccountState::rejected(RejectionKind::RateLimit);
        // 探测失败（None）表示「无新信息」，调用方据此保留旧状态
        assert!(AccountState::after_probe(Some(&previous), None).is_none());
    }
}
