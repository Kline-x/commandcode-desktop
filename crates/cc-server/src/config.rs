//! 连接级配置：上游地址、超时、协议偏好、功能开关。
//!
//! 与上游一致，所有默认值都来自实测（见 docs/PROTOCOL.md 与 docs/PLAN.md 第 4 节）。

use std::time::Duration;

/// 上游 API 默认基址。
pub const DEFAULT_API_BASE: &str = "https://api.commandcode.ai";

/// 官方 CLI 版本号，作为 `x-command-code-version` 上报。
///
/// 上游会校验该头；值需与真实 CLI 版本保持一致（可由控制面从 npm registry 刷新）。
pub const DEFAULT_CLI_VERSION: &str = "1.53.1";

/// 请求体里 `max_tokens` 的默认上限。
pub const DEFAULT_GENERATE_MAX_TOKENS: u64 = 64_000;

/// 模型目录请求超时。
pub const MODELS_TIMEOUT_MS: u64 = 10_000;

/// 等待上游响应头的默认超时（**不约束 body 流**）。
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// 流空闲超时：超过该时长没有任何事件即判定为死连接。
///
/// 默认给到 5 分钟是**有意**的：xhigh/max 档的推理模型可以静默思考很久，
/// 官方 CLI 干脆不设上限。过紧的超时会把健康的长时间思考误杀成 TIMEOUT。
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;

/// 支持的对外协议面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicProtocol {
    /// `/v1/chat/completions`（OpenAI）
    OpenAi,
    /// `/v1/messages`（Anthropic）
    Anthropic,
}

/// 上游通道选择。
///
/// `/provider/v1/*` 是文档化的接口（需 Pro+ 套餐）；`/alpha/generate` 是逆向所得、
/// **Go 套餐唯一可用**的通道。`Auto` 先试 Provider API，被 `upgrade_required`
/// 打回后固定降级到 CLI 通道并记忆一段时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpstreamProtocol {
    /// 自动：先 Provider API，遇 `upgrade_required` 降级到 CLI。
    #[default]
    Auto,
    /// 强制走 `/alpha/generate`。
    Cli,
    /// 优先 `/provider/v1/chat/completions`（仍会在 `upgrade_required` 时降级）。
    ProviderApi,
}

/// 服务配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 上游基址。
    pub api_base: String,
    /// 上报给上游的 CLI 版本号。
    pub cli_version: String,
    /// 本地监听地址。
    pub listen_addr: String,
    /// 等待响应头超时。
    pub request_timeout: Duration,
    /// 流空闲超时。
    pub stream_idle_timeout: Duration,
    /// 上游通道偏好。
    pub upstream_protocol: UpstreamProtocol,
    /// 无 system prompt 时是否发送空格占位。
    ///
    /// 上游在 `params.system` 缺省时会注入约 7.5K token 的默认提示词（且会让模型
    /// 以为自己在 CLI 的可执行目录里）。发一个空格即可绕过，实测 prompt_tokens
    /// 从 7653 降到 85。见 docs/PROTOCOL.md #3。
    pub empty_system_placeholder: bool,
    /// 是否请求 ZDR-only 路由（`x-cmd-zdr: 1`）。
    pub zdr: bool,
    /// `x-project-slug` 头。上游用于归属统计，填任意稳定值即可。
    pub project_slug: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
            cli_version: DEFAULT_CLI_VERSION.to_string(),
            listen_addr: "127.0.0.1:3050".to_string(),
            request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
            stream_idle_timeout: Duration::from_millis(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
            upstream_protocol: UpstreamProtocol::Auto,
            empty_system_placeholder: true,
            zdr: false,
            project_slug: "cc-desktop".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let c = Config::default();
        assert_eq!(c.api_base, "https://api.commandcode.ai");
        assert_eq!(c.request_timeout, Duration::from_millis(60_000));
        // 5 分钟是有意的：长思考模型会静默很久
        assert_eq!(c.stream_idle_timeout, Duration::from_millis(300_000));
        assert!(
            c.empty_system_placeholder,
            "空格占位默认开启（省 7.5K token）"
        );
        assert!(!c.zdr, "ZDR 需显式开启");
    }
}
