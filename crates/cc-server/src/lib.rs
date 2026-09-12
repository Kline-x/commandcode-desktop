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
//! - [pool]   — 账号池的可用性判定与选择规则
//!
//! 上游溯源：协议知识与轮换策略移植自 MIT 许可的社区项目，见 THIRD_PARTY.md。

pub mod config;
pub mod error;
pub mod pool;
pub mod sse;

pub use config::{Config, PublicProtocol, UpstreamProtocol};
pub use error::{CcError, ErrorEnvelope};
pub use pool::{
    account_usable, select_account_for_model, select_active_account, AccountPool, AccountSlot,
    AccountState, AccountStateKind, ModelAccountRule, RejectionKind, ResolvedAccount, Rotation,
    RotationStep, UsageTotals, WindowProbe, MAX_ACCOUNT_ROTATIONS, RETRY_MAX_DELAY_MS,
};
pub use sse::{LineBuffer, UpstreamEvent, Usage};
