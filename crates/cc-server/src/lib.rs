//! # cc-server — Command Code 客户端核心
//!
//! 本 crate 承载**与平台无关**的全部逻辑：上游协议翻译、账号池与轮换、
//! 配额解析、本地代理服务。它不依赖 Tauri，因此可以在 Linux CI 上以极低成本
//! 完成 cargo test / clippy（见 docs/PLAN.md 第 15 节）。
//!
//! Tauri 外壳（src-tauri）只是薄薄一层：窗口、托盘、密钥、进程编排。
//!
//! 模块地图：
//! - [config] — 连接级配置与默认值
//! - [error]  — 统一错误类型 + 对外错误信封；**轮换语义的判定依据**
//! - [sse]    — 上游 NDJSON/SSE 行解析与事件类型
//! - [pool]   — 账号池的可用性判定、选择规则与轮换状态机
//! - [proxy]  — 本地代理服务：端点、轮换循环、SSE 出流
//! - [quota]  — /alpha/* 配额端点的容错解析与月度额度派生（纯函数，无 I/O）
//! - [openai] — 上游事件 → OpenAI SSE 的有状态转换
//! - [time]   — epoch 毫秒 ↔ 公历日期（易错，故独立成模块）
//! - [upstream] — 上游 HTTP 客户端：伪装头、会话/指纹、请求发送与事件流
//!
//! 上游溯源：协议知识与轮换策略移植自 MIT 许可的社区项目，见 THIRD_PARTY.md。

pub mod config;
pub mod convert;
pub mod error;
pub mod openai;
pub mod pool;
pub mod proxy;
pub mod quota;
pub mod sse;
pub mod time;
pub mod upstream;

pub use config::{Config, PublicProtocol, UpstreamProtocol};
pub use convert::{
    build_generate_body, build_generate_body_with_context, extract_system_prompt, GenerateContext,
    MAX_GENERATE_TOKENS,
};
pub use error::{CcError, ErrorEnvelope};
pub use openai::{build_completion, to_sse_line, ChunkBuilder, SSE_DONE};
pub use pool::{
    account_usable, select_account_for_model, select_active_account, AccountPool, AccountSlot,
    AccountState, AccountStateKind, ModelAccountRule, RejectionKind, ResolvedAccount, Rotation,
    RotationStep, UsageTotals, WindowProbe, MAX_ACCOUNT_ROTATIONS, RETRY_MAX_DELAY_MS,
};
pub use proxy::{error_response, router, KeyResolver, ProxyState, RequestObserver, RequestRecord};
pub use quota::{
    build_snapshot, plan_display_name, plan_monthly_cap, window_reset_wait_ms, AccountIdentity,
    Credits, EndpointResponses, MonthlyQuota, QuotaAlert, QuotaSnapshot, WindowUsage,
    LOW_BALANCE_THRESHOLD,
};
pub use sse::{LineBuffer, UpstreamEvent, Usage};
pub use time::{civil_from_days, date_string, format_epoch_ms};
pub use upstream::{now_ms, EventStream, UpstreamClient};

// 测试专用：mock 上游服务（真实 axum HTTP 服务器 + 行为脚本 + 请求记录）。
// 由 cfg(test) / feature = "mock" 门控，不进入默认构建；理由见模块文档注释。
#[cfg(any(test, feature = "mock"))]
pub mod mock_upstream;
