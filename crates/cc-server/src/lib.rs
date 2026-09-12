//! # cc-server — Command Code 客户端核心
//!
//! 本 crate 承载**与平台无关**的全部逻辑：上游协议翻译、账号池与轮换、
//! 配额解析、存储。它不依赖 Tauri，因此可以在 Linux CI 上以极低成本完成
//! cargo test / clippy（见 docs/PLAN.md 第 15 节「CI 与成本策略」）。
//!
//! Tauri 外壳（src-tauri）只是薄薄一层：窗口、托盘、密钥、进程编排。
//!
//! 模块现状：
//! - pool — 账号池的可用性判定与选择规则（已就绪）
//!
//! 上游溯源：协议知识与轮换策略移植自 MIT 许可的社区项目，见 THIRD_PARTY.md。

pub mod pool;

/// 单次请求允许的最大账号轮换次数（与上游实现一致）。
pub use pool::MAX_ACCOUNT_ROTATIONS;
/// 全池耗尽时允许附加给客户端的最长重试等待。
pub use pool::RETRY_MAX_DELAY_MS;
