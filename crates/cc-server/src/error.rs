//! 统一错误类型与对外（OpenAI / Anthropic）错误信封的映射。
//!
//! 上游把多种业务拒绝折叠进少数 HTTP 状态码（403 里可能是套餐超限、模型不可用
//! 或 CLI 版本过低；402 额度耗尽会被报成 429）。因此错误类型必须携带**语义**
//! 而非仅状态码——轮换层要靠语义决定「换号有没有用」。见 docs/PROTOCOL.md 第 6 节。

use serde_json::json;

/// 面向客户端的错误信封格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorEnvelope {
    /// OpenAI 形状：`{ "error": { "message", "type", "code" } }`
    OpenAi,
    /// Anthropic 形状：`{ "type": "error", "error": { "type", "message" } }`
    Anthropic,
}

/// 核心错误类型。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CcError {
    /// 没有任何可用的 API key。
    #[error("llm-commandcode: 未找到可用的 API key；请在设置中填入 Command Code 密钥（user_ 开头），或运行 command-code login")]
    MissingCredential,

    /// 所有账号密钥都被拒绝（401）。
    #[error("Command Code API 返回 401：API 密钥缺失或无效——请在设置页检查密钥，或重新运行 command-code login")]
    InvalidCredential,

    /// 用量窗口耗尽（含全池耗尽）。`retry_after_ms` 是到最早窗口重置的等待。
    #[error("{message}")]
    RateLimit {
        message: String,
        retry_after_ms: Option<u64>,
    },

    /// 上游返回的其他 HTTP 错误。
    #[error("Command Code API error {status}{}: {body}", code_suffix(.code))]
    UpstreamHttp {
        status: u16,
        /// 上游 `error.code`（如 upgrade_required / MODEL_NOT_IN_PLAN），403 下尤其重要。
        code: Option<String>,
        body: String,
    },

    /// 传输层失败（DNS、连接被拒、TLS、代理、连接重置）。
    #[error("Command Code API 请求失败：{0}")]
    Transport(String),

    /// 等待响应头超时。
    #[error("Command Code API 请求在 {0} 毫秒内未收到响应——通常是网络或代理问题")]
    Timeout(u64),

    /// 流中途断开。
    #[error("Command Code API 流式响应中途断开——网络波动所致，重试通常可恢复：{0}")]
    StreamBroken(String),

    /// 流长时间无事件，判定为死连接。
    #[error("Command Code API 流式响应已 {0} 毫秒无任何事件，被判定为死连接——长思考模型可调大流空闲超时")]
    StreamIdle(u64),

    /// 客户端请求了本适配器不支持的能力（例如 stop 序列）。
    #[error("Command Code 不支持该请求选项：{0}")]
    UnsupportedOption(String),

    /// 内容类型不受支持（例如文本模型收到图片）。
    #[error("Command Code 模型 {model} 不支持图片输入；请改用支持视觉的模型，或先移除图片")]
    UnsupportedContent { model: String },

    /// 上游返回了无法解析的内容。
    #[error("Command Code 协议错误：{0}")]
    Protocol(String),

    /// 响应没有任何内容。
    #[error("Command Code 返回了空响应，重试通常可恢复")]
    EmptyResponse,
}

/// 给 `UpstreamHttp` 渲染 ` (code)` 后缀。
fn code_suffix(code: &Option<String>) -> String {
    code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
}

impl CcError {
    /// 该错误是否值得换一个账号重试。
    ///
    /// **这是轮换逻辑的核心判定**：只有「这个账号不行」才换号；
    /// 「这个请求本身不行」（模型不在套餐、参数非法）换号也没用，
    /// 换号反而会把整个账号池误标为耗尽。
    pub fn rotates_account(&self) -> bool {
        match self {
            CcError::InvalidCredential | CcError::RateLimit { .. } => true,
            CcError::UpstreamHttp { status, .. } => *status == 401 || *status == 429,
            _ => false,
        }
    }

    /// 上游是否在说「这个 key 没有 Provider API 权限，请改用 CLI 通道」。
    pub fn is_upgrade_required(&self) -> bool {
        match self {
            CcError::UpstreamHttp {
                status: 403,
                code,
                body,
            } => {
                let haystack = code.as_deref().unwrap_or(body);
                haystack.to_ascii_lowercase().contains("upgrade_required")
            }
            _ => false,
        }
    }

    /// 映射到 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        match self {
            CcError::MissingCredential | CcError::InvalidCredential => 401,
            CcError::RateLimit { .. } => 429,
            CcError::UpstreamHttp { status, .. } => *status,
            CcError::Timeout(_) | CcError::StreamIdle(_) => 504,
            CcError::Transport(_) | CcError::StreamBroken(_) => 502,
            CcError::UnsupportedOption(_) | CcError::UnsupportedContent { .. } => 400,
            CcError::Protocol(_) | CcError::EmptyResponse => 502,
        }
    }

    /// 面向 SDK 的稳定错误码（OpenAI 的 `code` / Anthropic 的 `type`）。
    pub fn code(&self) -> &'static str {
        match self {
            CcError::MissingCredential | CcError::InvalidCredential => "invalid_api_key",
            CcError::RateLimit { .. } => "rate_limit_exceeded",
            CcError::UnsupportedOption(_) | CcError::UnsupportedContent { .. } => {
                "invalid_request_error"
            }
            CcError::Timeout(_) | CcError::StreamIdle(_) | CcError::StreamBroken(_) => {
                "upstream_timeout"
            }
            CcError::EmptyResponse => "empty_response",
            CcError::UpstreamHttp { .. } => "upstream_error",
            CcError::Transport(_) => "upstream_unavailable",
            CcError::Protocol(_) => "protocol_error",
        }
    }

    /// 该错误附带的重试等待（毫秒）。全池耗尽时会带上「最早重置时间」。
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            CcError::RateLimit { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }

    /// 渲染为客户端要的错误信封。
    pub fn to_envelope(&self, envelope: ErrorEnvelope) -> serde_json::Value {
        let message = self.to_string();
        match envelope {
            ErrorEnvelope::OpenAi => json!({
                "error": {
                    "message": message,
                    "type": self.code(),
                    "code": self.code(),
                }
            }),
            ErrorEnvelope::Anthropic => json!({
                "type": "error",
                "error": { "type": self.code(), "message": message },
            }),
        }
    }

    /// 构造全池耗尽错误。
    ///
    /// `earliest_reset_ms` 是各账号窗口重置时间中最早的一个（epoch 毫秒）。
    /// 附带的等待**必须**不超过重试策略的 `max_delay`——超过上限的「附带等待」
    /// 会让重试执行器直接放弃重试，把「等到窗口开放」变成「立刻失败」。
    pub fn all_accounts_exhausted(
        count: usize,
        earliest_reset_ms: Option<i64>,
        now_ms: i64,
    ) -> Self {
        if count == 0 {
            return CcError::MissingCredential;
        }
        let wait = earliest_reset_ms
            .map(|reset| (reset - now_ms).max(1000))
            .map(|w| w as u64)
            .filter(|w| *w <= crate::pool::RETRY_MAX_DELAY_MS);
        let note = earliest_reset_ms
            .map(|reset| format!("；最早的窗口将于 {} 重置", format_epoch_ms(reset)))
            .unwrap_or_default();
        CcError::RateLimit {
            message: format!(
                "已用尽全部 {count} 个 Command Code 账户的用量窗口{note}——窗口重置后请求会自动恢复（也可以添加更多账户）"
            ),
            retry_after_ms: wait,
        }
    }

    /// 便利构造：普通限流。
    pub fn rate_limited(message: impl Into<String>) -> Self {
        CcError::RateLimit {
            message: message.into(),
            retry_after_ms: None,
        }
    }
}

/// 把 epoch 毫秒格式化为可读的 UTC 时间（错误信息里给人看）。
///
/// 不引入 chrono：用 Howard Hinnant 的 civil_from_days 算法自行换算，
/// 保持依赖精简（本项目目标是单二进制）。
fn format_epoch_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_failures_do_not_rotate_accounts() {
        // 网络断了换号也没用，不该把池里的账号全标掉
        assert!(!CcError::Transport("reset".into()).rotates_account());
        assert!(!CcError::Timeout(60_000).rotates_account());
        assert!(!CcError::EmptyResponse.rotates_account());
    }

    #[test]
    fn credential_and_rate_limit_rotate() {
        assert!(CcError::InvalidCredential.rotates_account());
        assert!(CcError::UpstreamHttp {
            status: 401,
            code: None,
            body: String::new()
        }
        .rotates_account());
        assert!(CcError::UpstreamHttp {
            status: 429,
            code: None,
            body: String::new()
        }
        .rotates_account());
    }

    #[test]
    fn plan_errors_must_not_rotate() {
        // PROTOCOL.md #6：模型不在套餐的 403 换号无用，且会误伤整个池
        let err = CcError::UpstreamHttp {
            status: 403,
            code: Some("MODEL_NOT_IN_PLAN".into()),
            body: "{}".into(),
        };
        assert!(!err.rotates_account(), "403 套餐错误不得触发换号");
        assert!(!err.is_upgrade_required());
    }

    #[test]
    fn upgrade_required_is_detected_from_code_or_body() {
        let by_code = CcError::UpstreamHttp {
            status: 403,
            code: Some("upgrade_required".into()),
            body: "{}".into(),
        };
        assert!(by_code.is_upgrade_required());

        let by_body = CcError::UpstreamHttp {
            status: 403,
            code: None,
            body: r#"{"error":{"code":"upgrade_required"}}"#.into(),
        };
        assert!(by_body.is_upgrade_required());
    }

    #[test]
    fn wait_is_capped_at_retry_max_delay() {
        let now = 1_000_000;
        // 远超上限的等待必须丢弃：否则重试执行器会直接放弃重试
        let far = CcError::all_accounts_exhausted(2, Some(now + 10_000_000), now);
        assert_eq!(
            far.retry_after_ms(),
            None,
            "超过 RETRY_MAX_DELAY_MS 的等待应被丢弃"
        );

        // 上限内的等待要保留
        let soon = CcError::all_accounts_exhausted(2, Some(now + 60_000), now);
        assert_eq!(soon.retry_after_ms(), Some(60_000));
    }

    #[test]
    fn zero_accounts_is_a_credential_problem() {
        assert_eq!(
            CcError::all_accounts_exhausted(0, None, 0),
            CcError::MissingCredential
        );
    }

    #[test]
    fn envelope_shapes_match_sdk_expectations() {
        let err = CcError::MissingCredential;
        let openai = err.to_envelope(ErrorEnvelope::OpenAi);
        assert_eq!(openai["error"]["code"], "invalid_api_key");
        let anthropic = err.to_envelope(ErrorEnvelope::Anthropic);
        assert_eq!(anthropic["type"], "error");
        assert_eq!(anthropic["error"]["type"], "invalid_api_key");
    }

    #[test]
    fn upstream_http_renders_code_suffix() {
        let err = CcError::UpstreamHttp {
            status: 403,
            code: Some("MODEL_NOT_IN_PLAN".into()),
            body: "nope".into(),
        };
        assert!(err.to_string().contains("(MODEL_NOT_IN_PLAN)"));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
    }
}
