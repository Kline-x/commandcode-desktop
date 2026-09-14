//! 本地代理服务：对外 OpenAI 兼容端点，对内驱动账号池 + 上游。
//!
//! 一次请求的完整生命周期（见 docs/ARCHITECTURE.md 第 3 节）：
//!
//! 1. 解析请求体（保留未知字段）；
//! 2. 从池中选号（模型路由 → 手动指定 → 轮转首个可用）；
//! 3. 转换请求体（[crate::convert]）；
//! 4. 发送上游（带伪装头，[crate::upstream]）；
//! 5. 若在**出流前**遇到 401/429 → 标记该 key 并换号重试（每 key 一次，有上限）；
//!    若遇 403 upgrade_required → 记住该 key 只能走 CLI 通道并原样重试；
//! 6. 把上游事件流翻译成 OpenAI SSE（[crate::openai]）；
//! 7. 收尾：记录用量与结果。
//!
//! **只在出流前轮换**：一旦响应头已发出（客户端已收到 200），就不能再换号重放——
//! 客户端会看到重复内容。中途断流按错误处理，不重放。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::config::{Config, PublicProtocol, UpstreamProtocol};
use crate::convert::{build_generate_body_with_context, GenerateContext};
use crate::error::{CcError, ErrorEnvelope};
use crate::openai::{build_completion, to_sse_line, ChunkBuilder, SSE_DONE};
use crate::pool::{
    AccountPool, AccountSlot, ModelAccountRule, RejectionKind, ResolvedAccount, Rotation,
    RotationStep, UsageTotals, WindowProbe,
};
use crate::upstream::{now_ms, EventStream, UpstreamClient};

/// 账号的凭据解析器：给定槽位，返回其 API key。
///
/// 由宿主（Tauri 或测试）注入：cc-server 不关心 key 存在哪里（keychain / 内存 / 文件）。
pub type KeyResolver = Arc<dyn Fn(&AccountSlot) -> Option<String> + Send + Sync>;

/// 请求完成后的回调（用于落库与推送到 UI）。
pub type RequestObserver = Arc<dyn Fn(RequestRecord) + Send + Sync>;

/// 把一次配额快照换算成账号池能消费的窗口探测结果。
///
/// 返回值的三态**必须**区分清楚，否则一次失败的探测会把账号错误地复活：
/// - `Some(probe)` — 拿到了窗口数据，可以据此改状态；
/// - `None` — 两个窗口都不在快照里（credits 端点失败或字段漂移），
///   **没有信息**，调用方必须保留旧状态。
///
/// 超限时取两个窗口里**最早**的重置时刻：`exhausted_error` 要用它告诉客户端
/// 「什么时候可以重试」，取晚了会让客户端睡过头。
pub fn window_probe_from_snapshot(snapshot: &crate::quota::QuotaSnapshot) -> Option<WindowProbe> {
    let windows = [snapshot.five_hour.as_ref(), snapshot.weekly.as_ref()];
    let present: Vec<&crate::quota::WindowUsage> = windows.into_iter().flatten().collect();
    if present.is_empty() {
        // 没有窗口数据：探测失败不携带信息（docs/PROTOCOL.md 第 7 节）
        return None;
    }
    let exceeded: Vec<&&crate::quota::WindowUsage> =
        present.iter().filter(|w| w.exceeded).collect();
    if exceeded.is_empty() {
        // 有窗口数据且都未超限 → 明确「这个账号现在可用」，清除标记
        return Some(WindowProbe {
            exceeded: false,
            reset_at_ms: 0,
        });
    }
    let earliest_reset = exceeded
        .iter()
        .map(|w| w.reset_at_ms)
        .filter(|reset| *reset > 0)
        .min()
        .unwrap_or(0);
    Some(WindowProbe {
        exceeded: true,
        reset_at_ms: earliest_reset,
    })
}

/// 计算本次请求预估的消耗金额（美元）。
///
/// 价格参考主流模型公开定价（按 1M tokens 换算）。
/// 包含未缓存输入、提示词缓存命中输入（通常 10%~50% 成本）、输出补全三部分。
pub fn estimate_cost_usd(
    model: &str,
    status: u16,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
) -> f64 {
    if status >= 400 || (input_tokens == 0 && output_tokens == 0) {
        return 0.0;
    }
    let m = model.to_lowercase();
    // (input_per_m, cached_input_per_m, output_per_m)
    let (p_in, p_cache, p_out) = if m.contains("claude-3-7-sonnet")
        || m.contains("claude-3.7-sonnet")
        || m.contains("claude-3-5-sonnet")
        || m.contains("claude-3.5-sonnet")
    {
        (3.0, 0.30, 15.0)
    } else if m.contains("claude-3-5-haiku") || m.contains("claude-3-haiku") {
        (0.80, 0.08, 4.0)
    } else if m.contains("claude-3-opus") {
        (15.0, 1.50, 75.0)
    } else if m.contains("gpt-4o-mini") {
        (0.15, 0.075, 0.60)
    } else if m.contains("gpt-4o") {
        (2.50, 1.25, 10.0)
    } else if m.contains("o1-mini") || m.contains("o3-mini") {
        (1.10, 0.55, 4.40)
    } else if m.contains("o1") {
        (15.0, 7.50, 60.0)
    } else if m.contains("deepseek-v4-pro")
        || m.contains("deepseek-reasoner")
        || m.contains("deepseek-r1")
    {
        (0.55, 0.14, 2.19)
    } else if m.contains("deepseek-v4-flash")
        || m.contains("deepseek-chat")
        || m.contains("deepseek-v3")
    {
        (0.14, 0.014, 0.28)
    } else if m.contains("gemini-2.0-flash") || m.contains("gemini-1.5-flash") {
        (0.10, 0.025, 0.40)
    } else if m.contains("gemini-2.0-pro") || m.contains("gemini-1.5-pro") {
        (1.25, 0.3125, 5.00)
    } else {
        // 兜底常用均价：$1.0 / M in, $0.25 / M cache, $3.0 / M out
        (1.0, 0.25, 3.0)
    };

    let uncached_in = input_tokens.saturating_sub(cached_tokens);
    (uncached_in as f64 * p_in + cached_tokens as f64 * p_cache + output_tokens as f64 * p_out)
        / 1_000_000.0
}

/// 一次请求的结果记录。
#[derive(Debug, Clone)]
pub struct RequestRecord {
    /// 完成时刻（epoch 毫秒）。
    pub at_ms: i64,
    /// 使用的账号槽位 id。
    pub account_id: String,
    /// 使用的账号展示名。
    pub account_label: String,
    /// 模型。
    pub model: String,
    /// 上游通道。
    pub protocol: &'static str,
    /// 客户端协议（openai_chat / anthropic / openai_responses）。
    pub client_protocol: &'static str,
    /// 是否流式。
    pub stream: bool,
    /// 最终 HTTP 状态。
    pub status: u16,
    /// 错误码（成功时为 None）。
    pub error_code: Option<String>,
    /// 用量合计（可能跨轮换累加）。
    pub usage: UsageTotals,
    /// 尝试过的账号次数。
    pub attempts: usize,
    /// 到首字节的耗时（毫秒）；未收到时为 None。
    pub ttft_ms: Option<i64>,
    /// 总耗时（毫秒）。
    pub total_ms: i64,
    /// 预估消耗金额（美元）。
    pub cost_usd: f64,
}

/// 代理服务的共享状态。
pub struct ProxyState {
    /// 配置。
    pub config: Config,
    /// 上游客户端。
    ///
    /// 用 Arc 持有：配额轮询器要共享同一个实例（会话/指纹状态才能一致）。
    pub upstream: Arc<UpstreamClient>,
    /// 账号池（轮换状态）。
    pub pool: std::sync::Mutex<AccountPool>,
    /// 槽位列表（配置层事实）。
    pub slots: std::sync::RwLock<Vec<AccountSlot>>,
    /// 凭据解析器。
    pub resolve_key: std::sync::RwLock<KeyResolver>,
    /// 模型 → 账号路由规则。
    pub rules: std::sync::RwLock<Vec<ModelAccountRule>>,
    /// 手动指定的账号 id。
    pub preferred_id: std::sync::RwLock<Option<String>>,
    /// 请求完成回调。
    pub observer: Option<RequestObserver>,
}

impl ProxyState {
    /// 组装一个新的代理状态。
    pub fn new(
        config: Config,
        slots: Vec<AccountSlot>,
        resolve_key: KeyResolver,
    ) -> Result<Self, CcError> {
        let upstream = UpstreamClient::new(config.clone())?;
        Ok(Self {
            config,
            upstream: Arc::new(upstream),
            pool: std::sync::Mutex::new(AccountPool::new()),
            slots: std::sync::RwLock::new(slots),
            resolve_key: std::sync::RwLock::new(resolve_key),
            rules: std::sync::RwLock::new(Vec::new()),
            preferred_id: std::sync::RwLock::new(None),
            observer: None,
        })
    }

    /// 设置模型路由规则。
    pub fn with_rules(self, rules: Vec<ModelAccountRule>) -> Self {
        *self.rules.write().unwrap_or_else(|e| e.into_inner()) = rules;
        self
    }

    /// 设置手动指定的账号。
    pub fn with_preferred(self, id: Option<String>) -> Self {
        *self.preferred_id.write().unwrap_or_else(|e| e.into_inner()) = id;
        self
    }

    /// 设置请求完成回调。
    pub fn with_observer(mut self, observer: RequestObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// 热重载账号槽位与密钥解析器。
    pub fn set_accounts(&self, slots: Vec<AccountSlot>, resolve_key: KeyResolver) {
        *self.slots.write().unwrap_or_else(|e| e.into_inner()) = slots;
        *self.resolve_key.write().unwrap_or_else(|e| e.into_inner()) = resolve_key;
    }

    /// 热重载模型路由规则。
    pub fn set_rules(&self, rules: Vec<ModelAccountRule>) {
        *self.rules.write().unwrap_or_else(|e| e.into_inner()) = rules;
    }

    /// 解析出当前可用的账号列表。
    fn resolved(&self) -> Vec<ResolvedAccount> {
        let slots = self.slots.read().unwrap_or_else(|e| e.into_inner());
        let resolve = self.resolve_key.read().unwrap_or_else(|e| e.into_inner());
        // clippy 建议写成 map(**resolve)，但 resolve 是 RwLockReadGuard<Arc<dyn Fn>>，
        // 双重解引用会掩盖「这是一次 keychain 查询」的语义；显式闭包更清楚。
        #[allow(clippy::redundant_closure)]
        let keys: Vec<Option<String>> = slots.iter().map(|slot| resolve(slot)).collect();
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        pool.resolve(&slots, &keys)
    }

    /// 选出本次请求要用的账号，没有可用账号时返回池耗尽错误。
    fn select_account(&self, model: &str) -> Result<ResolvedAccount, CcError> {
        let accounts = self.resolved();
        if accounts.is_empty() {
            return Err(CcError::MissingCredential);
        }
        let rules = self.rules.read().unwrap_or_else(|e| e.into_inner());
        let preferred = self.preferred_id.read().unwrap_or_else(|e| e.into_inner());
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        match pool.select(&accounts, model, &rules, preferred.as_deref(), now_ms()) {
            Some(found) => Ok(found.clone()),
            None => Err(pool.exhausted_error(&accounts, now_ms())),
        }
    }

    /// 标记某个 key 被拒绝。
    fn mark_rejected(&self, key: &str, kind: RejectionKind) {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        pool.mark_rejected(key, kind);
    }

    /// 用一次配额探测的结果更新账号池的可用性。
    ///
    /// **这是「429 标记的账号能自己活过来」的唯一路径。** 在此之前
    /// [`AccountPool::apply_probe`] 只有测试调用过：账号一旦被 429 标记成
    /// `Unknown`（不可用），就再没有任何生产代码能把标记清掉——只能重启进程。
    /// 而错误文案偏偏承诺「窗口重置后请求会自动恢复」，与实际行为相反。
    ///
    /// 语义（由 [`AccountState::after_probe`] 保证）：
    /// - 探测成功且窗口**未**超限 → 清除标记（账号复活）；
    /// - 探测成功且窗口**仍**超限 → 记录 cooldown（带准确的重置时刻，
    ///   比 429 当时的 `Unknown` 更有信息量，`exhausted_error` 能给出最早重置时间）；
    /// - 探测失败（`probe` 为 None）→ **不改变状态**，失败不携带信息
    ///   （docs/PROTOCOL.md 第 7 节：探针失败不得改变账号池状态）。
    ///
    /// 探测结果按**槽位 id** 传入，内部换算成 key：池的状态以 key 为键，
    /// 而配额轮询器只知道槽位 id（它不该碰 key，见 quota_poller 的模块注释）。
    pub fn apply_probe_for_slot(&self, slot_id: &str, probe: Option<WindowProbe>) {
        let slots = self.slots.read().unwrap_or_else(|e| e.into_inner());
        let resolve = self.resolve_key.read().unwrap_or_else(|e| e.into_inner());
        let slot = slots.iter().find(|slot| slot.id == slot_id);
        // 与 [Self::resolved] 同一处理：clippy 建议写 `&**resolve`，但双重解引用会
        // 掩盖「这是一次 keychain 查询」的语义；显式闭包更清楚。
        #[allow(clippy::redundant_closure)]
        let key = slot.and_then(|found| resolve(found));
        let Some(key) = key else {
            // 账号已被删除或凭据不可解析：没有可标记的对象
            return;
        };
        drop(slots);
        drop(resolve);

        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        // 健康账号 + 探测失败：没有新信息，不必写池（也避免覆盖刚标记的状态）
        if pool.state(&key).is_none() && probe.is_none() {
            return;
        }
        let revived = probe.as_ref().is_some_and(|p| !p.exceeded) && pool.state(&key).is_some();
        pool.apply_probe(&key, probe);
        if revived {
            tracing::info!(slot_id, "账号窗口已重置，重新纳入轮换");
        }
    }
}

/// 构造路由。
pub fn router(state: Arc<ProxyState>) -> Router {
    // 请求体上限必须显式放宽：axum 的 Bytes 提取器默认只收 2 MB，
    // 而编码代理的大上下文请求远超这个量级（见 Config::max_body_bytes）。
    let body_limit = axum::extract::DefaultBodyLimit::max(state.config.max_body_bytes);
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/messages", post(messages))
        .route("/v1/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/v1/responses", post(responses))
        .layer(body_limit)
        .with_state(state)
}

/// 健康检查。
async fn health(State(state): State<Arc<ProxyState>>) -> Response {
    let count = state.slots.read().unwrap_or_else(|e| e.into_inner()).len();
    axum::Json(json!({
        "status": "ok",
        "accounts": count,
        "api_base": state.config.api_base,
    }))
    .into_response()
}

/// 模型目录：用池中第一个能解析出 key 的账号去取。
async fn list_models(State(state): State<Arc<ProxyState>>) -> Response {
    let Some(account) = state.resolved().first().cloned() else {
        return error_response(&CcError::MissingCredential, ErrorEnvelope::OpenAi);
    };
    match state.upstream.list_models(&account.key).await {
        Ok(value) => axum::Json(value).into_response(),
        Err(e) => error_response(&e, ErrorEnvelope::OpenAi),
    }
}

/// 把错误渲染成 HTTP 响应。
pub fn error_response(error: &CcError, envelope: ErrorEnvelope) -> Response {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = axum::Json(error.to_envelope(envelope)).into_response();
    *response.status_mut() = status;
    if let Some(wait) = error.retry_after_ms() {
        if let Ok(value) = HeaderValue::from_str(&(wait / 1000).max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

/// 从请求头里取一个字符串值。
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// 从入站请求里提取客户端自带的会话标识。
///
/// 上游按 session 组织前缀缓存。客户端若已自带会话 id（Claude Code 等），
/// 透传它能让同一会话的请求归到一组，提高缓存命中率；缺失时由上游客户端
/// 按 key 生成一个稳定默认值（见 [crate::upstream::UpstreamClient::session_id]）。
///
/// 最短长度要求（>= 8）沿用参考实现：过短的 id 多是客户端占位符，
/// 透传反而会污染缓存分组。
fn inbound_session_id(headers: &HeaderMap) -> Option<String> {
    const CANDIDATES: [&str; 3] = ["x-session-id", "x-claude-code-session-id", "session_id"];
    CANDIDATES
        .iter()
        .find_map(|name| header_str(headers, name))
        .filter(|value| value.len() >= 8)
        .map(str::to_string)
}

/// 入口：OpenAI 兼容的 chat completions。
async fn chat_completions(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                &CcError::Protocol(format!("请求体不是合法 JSON：{e}")),
                ErrorEnvelope::OpenAi,
            );
        }
    };
    // 智能嗅探：若带有 anthropic-version 请求头，说明客户端误将 Anthropic Base URL 配置到了 /v1/chat/completions
    if headers.contains_key("anthropic-version") {
        let converted = match crate::anthropic::anthropic_to_openai(&request) {
            Ok(c) => c,
            Err(e) => return error_response(&e, ErrorEnvelope::Anthropic),
        };
        return run_generation(state, headers, converted, PublicProtocol::Anthropic).await;
    }

    // 智能嗅探：若请求体包含 input 且无 messages，说明客户端以 Responses API 结构发送至此
    if request.get("input").is_some() && request.get("messages").is_none() {
        let converted = match crate::responses::convert_responses_to_chat(&request) {
            Ok(c) => c,
            Err(e) => return error_response(&e, ErrorEnvelope::OpenAi),
        };
        return run_generation(state, headers, converted, PublicProtocol::Responses).await;
    }

    // OpenAI 形状直接进入统一生成流程
    run_generation(state, headers, request, PublicProtocol::OpenAi).await
}

/// 入口：Anthropic 兼容的 messages。
///
/// 先把 Anthropic 请求转成 OpenAI 形状（复用 [crate::anthropic::anthropic_to_openai]），
/// 之后与 [chat_completions] 走**完全同一条**生成流程——账号轮换、错误语义、
/// 用量统计都只有一份实现。
async fn messages(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let incoming: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                &CcError::Protocol(format!("请求体不是合法 JSON：{e}")),
                ErrorEnvelope::Anthropic,
            );
        }
    };
    let request = match crate::anthropic::anthropic_to_openai(&incoming) {
        Ok(converted) => converted,
        Err(e) => return error_response(&e, ErrorEnvelope::Anthropic),
    };
    run_generation(state, headers, request, PublicProtocol::Anthropic).await
}

/// 入口：OpenAI Responses API（/v1/responses）。
///
/// 供 Codex 等使用 Responses 协议的客户端接入。代理作为无状态转换层：
/// 把 input 翻译成内部 Chat 格式，复用同一套 CC 转发管线。
async fn responses(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let incoming: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                &CcError::Protocol(format!("请求体不是合法 JSON：{e}")),
                ErrorEnvelope::OpenAi,
            );
        }
    };
    let request = match crate::responses::convert_responses_to_chat(&incoming) {
        Ok(converted) => converted,
        Err(e) => return error_response(&e, ErrorEnvelope::OpenAi),
    };
    run_generation(state, headers, request, PublicProtocol::Responses).await
}

/// 统一的生成流程（三种对外协议共用）。
///
/// public 决定**响应的编码形状**（OpenAI SSE / Anthropic SSE / Responses SSE），
/// 而请求体在进入本函数前已经统一成 OpenAI 形状。
async fn run_generation(
    state: Arc<ProxyState>,
    headers: HeaderMap,
    request: Value,
    public: PublicProtocol,
) -> Response {
    let envelope = match public {
        PublicProtocol::OpenAi | PublicProtocol::Responses => ErrorEnvelope::OpenAi,
        PublicProtocol::Anthropic => ErrorEnvelope::Anthropic,
    };
    let started = now_ms();

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 客户端自带的会话 id 会被透传给上游（提升前缀缓存命中），
    // 这里先取出；缺失时由上游客户端按 key 生成稳定默认值。
    let client_session = inbound_session_id(&headers);

    // 上游只有流式接口；非流式请求由本地缓冲后一次性返回
    let first = match state.select_account(&model) {
        Ok(account) => account,
        Err(e) => {
            // 没有可用账号也要记一条流水：这是首次使用时最常见的状态，
            // 面板上什么都不显示会让用户以为程序坏了。
            let placeholder = ResolvedAccount {
                slot: AccountSlot {
                    id: "(无账号)".to_string(),
                    label: String::new(),
                },
                key: String::new(),
                state: None,
            };
            record(
                &state,
                &placeholder,
                &model,
                UpstreamProtocol::Auto,
                public,
                stream,
                &e,
                &UsageTotals::default(),
                1,
                None,
                started,
            );
            return error_response(&e, envelope);
        }
    };

    let completion_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = started / 1000;
    let mut rotation = Rotation::start(first.key.clone());
    let mut current = first;
    let mut usage_totals = UsageTotals::default();
    let mut protocol = state.upstream.initial_protocol(&current.key);
    let mut ttft_ms: Option<i64> = None;
    // 传输层错误的载体：只在「拿不到响应」时被写入，写入后立即 break。
    // 网络断了换号也没用，所以它不参与轮换。
    // 成功路径一律 return 离开循环，因此 break 到这里时它必然已被赋值。
    let transport_error: Option<CcError>;

    loop {
        // 构造请求体（CLI 与 Provider 面共用同一份转换结果）
        let context = GenerateContext {
            working_dir: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            date: crate::time::date_string(now_ms()),
            environment: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            ..Default::default()
        };
        let generate_body =
            match build_generate_body_with_context(&request, &state.config, &context) {
                Ok(b) => b,
                Err(e) => return error_response(&e, envelope),
            };

        // 指纹预请求是尽力而为的，失败不影响对话
        state.upstream.ensure_initialized(&current.key).await;
        // 客户端会话 id 若存在，优先于按 key 生成的默认 session
        if let Some(session) = &client_session {
            state.upstream.adopt_session(&current.key, session);
        }

        let response = match state
            .upstream
            .send_generate(&current.key, protocol, &generate_body)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                transport_error = Some(e);
                break;
            }
        };

        if !response.status().is_success() {
            let error = UpstreamClient::classify_error(response).await;

            // Go 套餐没有 Provider API 权限：记住并原样重试同一个 key
            if error.is_upgrade_required() && protocol == UpstreamProtocol::ProviderApi {
                state.upstream.remember_cli_only(&current.key);
                protocol = UpstreamProtocol::Cli;
                continue;
            }

            // 只有「账号相关」的错误才标记并换号。
            //
            // ⚠️ 标记必须在**判定之后**：`mark_rejected` 会让该 key 变成不可用，
            // 而 403「模型不在套餐」这类**请求侧**错误换号无用。此前无条件标记会把
            // 一次模型不支持的请求变成「这个账号以后都不能用了」——配合
            // docs/PLAN.md 第 8.1 节「换号无用反而误伤整个池」的警告，这是同一类
            // 缺陷的副作用版本（判定写对了，副作用没跟上）。
            if error.rotates_account() {
                state.mark_rejected(
                    &current.key,
                    if error.http_status() == 401 {
                        RejectionKind::InvalidCredential
                    } else {
                        RejectionKind::RateLimit
                    },
                );
            }
            let next = next_account(&state, &rotation);
            match rotation.on_failure(&error, next) {
                RotationStep::Switched { key, .. } => {
                    current = state
                        .resolved()
                        .into_iter()
                        .find(|a| a.key == key)
                        .unwrap_or(current);
                    protocol = state.upstream.initial_protocol(&current.key);
                    continue;
                }
                RotationStep::Exhausted => {
                    // 全池都试过了：报告池耗尽的语义错误（带最早重置时间）
                    let accounts = state.resolved();
                    let pool = state.pool.lock().unwrap_or_else(|e| e.into_inner());
                    let final_error = if accounts.is_empty() {
                        error
                    } else if error.rotates_account() {
                        pool.exhausted_error(&accounts, now_ms())
                    } else {
                        error
                    };
                    drop(pool);
                    record(
                        &state,
                        &current,
                        &model,
                        protocol,
                        public,
                        stream,
                        &final_error,
                        &usage_totals,
                        rotation.attempted(),
                        ttft_ms,
                        started,
                    );
                    return error_response(&final_error, envelope);
                }
                RotationStep::NotRotatable => {
                    record(
                        &state,
                        &current,
                        &model,
                        protocol,
                        public,
                        stream,
                        &error,
                        &usage_totals,
                        rotation.attempted(),
                        ttft_ms,
                        started,
                    );
                    return error_response(&error, envelope);
                }
            }
        }

        // --- 成功：开始翻译事件流 ---
        let mut events = EventStream::new(response, state.config.stream_idle_timeout);
        if !stream {
            // 非流式：缓冲全部内容后一次性返回
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut finish = "stop".to_string();
            loop {
                match events.next_event().await {
                    Ok(Some(event)) => {
                        collect_event(
                            &event,
                            &mut content,
                            &mut reasoning,
                            &mut tool_calls,
                            &mut finish,
                        );
                    }
                    Ok(None) => break,
                    Err(e) => {
                        record(
                            &state,
                            &current,
                            &model,
                            protocol,
                            public,
                            stream,
                            &e,
                            &usage_totals,
                            rotation.attempted(),
                            ttft_ms,
                            started,
                        );
                        return error_response(&e, envelope);
                    }
                }
            }
            if let Some(u) = events.usage() {
                usage_totals.add(u);
            }
            // 按对外协议选择响应形状：请求体早已统一成 OpenAI 形状，
            // 只有**响应编码**需要区分（Anthropic 客户端认不得 OpenAI 的 JSON）。
            let body = match public {
                PublicProtocol::OpenAi => build_completion(
                    &completion_id,
                    created,
                    &model,
                    &content,
                    &reasoning,
                    &tool_calls,
                    &finish,
                    events.usage(),
                ),
                PublicProtocol::Anthropic => crate::anthropic::build_anthropic_response(
                    &completion_id,
                    &model,
                    &content,
                    &reasoning,
                    &tool_calls,
                    &finish,
                    events.usage(),
                ),
                PublicProtocol::Responses => crate::responses::build_responses_object(
                    &completion_id,
                    &model,
                    created,
                    &content,
                    &reasoning,
                    &tool_calls,
                    &finish,
                    events.usage().as_ref(),
                ),
            };
            let cost_usd = estimate_cost_usd(
                &model,
                200,
                usage_totals.input_tokens,
                usage_totals.output_tokens,
                usage_totals.cached_input_tokens,
            );
            let record_ok = RequestRecord {
                at_ms: now_ms(),
                account_id: current.slot.id.clone(),
                account_label: current.slot.label.clone(),
                model: model.clone(),
                protocol: protocol_name(protocol),
                client_protocol: public.as_str(),
                stream,
                status: 200,
                error_code: None,
                usage: usage_totals,
                attempts: rotation.attempted(),
                ttft_ms: Some(now_ms() - started),
                total_ms: now_ms() - started,
                cost_usd,
            };
            if let Some(observer) = &state.observer {
                observer(record_ok);
            }
            return axum::Json(body).into_response();
        }

        // 流式：把事件流翻译成 SSE
        // 三种协议各有一个有状态编码器；请求体已统一，这里只决定 SSE 形状。
        let mut openai_builder = ChunkBuilder::new(completion_id.clone(), created, model.clone());
        let mut anthropic_builder =
            crate::anthropic::AnthropicSseBuilder::new(completion_id.clone(), model.clone());
        let mut responses_builder = crate::responses::ResponsesSseBuilder::new(
            completion_id.clone(),
            created,
            model.clone(),
        );
        let stream_state = state.clone();
        let account = current.clone();
        let protocol_name = protocol_name(protocol);
        let model_for_record = model.clone();
        let mut totals = usage_totals;
        let mut saw_any = false;

        let output = async_stream::stream! {
            loop {
                match events.next_event().await {
                    Ok(Some(event)) => {
                        if ttft_ms.is_none() {
                            ttft_ms = Some(now_ms() - started);
                        }
                        saw_any = true;
                        match public {
                            PublicProtocol::OpenAi => {
                                for chunk in openai_builder.push(&event) {
                                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                                        to_sse_line(&chunk),
                                    ));
                                }
                            }
                            PublicProtocol::Anthropic => {
                                for sse in anthropic_builder.push(&event) {
                                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                                        sse.to_sse(),
                                    ));
                                }
                            }
                            PublicProtocol::Responses => {
                                for sse in responses_builder.push(&event) {
                                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                                        sse,
                                    ));
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // 已经出流，无法重放：以 SSE 里的错误事件结束连接
                        let payload = json!({
                            "error": { "message": e.to_string(), "type": e.code() }
                        });
                        match public {
                            PublicProtocol::OpenAi => {
                                yield Ok(Bytes::from(format!("data: {payload}\n\n")));
                            }
                            PublicProtocol::Anthropic => {
                                yield Ok(Bytes::from(format!("event: error\ndata: {payload}\n\n")));
                            }
                            PublicProtocol::Responses => {
                                for sse in responses_builder.fail(&e.to_string()) {
                                    yield Ok(Bytes::from(sse));
                                }
                            }
                        }
                        let cost_usd = estimate_cost_usd(
                            &model_for_record,
                            200,
                            totals.input_tokens,
                            totals.output_tokens,
                            totals.cached_input_tokens,
                        );
                        let rec = RequestRecord {
                            at_ms: now_ms(),
                            account_id: account.slot.id.clone(),
                            account_label: account.slot.label.clone(),
                            model: model_for_record.clone(),
                            protocol: protocol_name,
                            client_protocol: public.as_str(),
                            stream: true,
                            status: 200,
                            error_code: Some(e.code().to_string()),
                            usage: totals,
                            attempts: rotation.attempted(),
                            ttft_ms,
                            total_ms: now_ms() - started,
                            cost_usd,
                        };
                        if let Some(observer) = &stream_state.observer {
                            observer(rec);
                        }
                        return;
                    }
                }
            }
            if let Some(u) = events.usage() {
                totals.add(u);
            }
            if !saw_any {
                let e = CcError::EmptyResponse;
                let payload = json!({
                    "error": { "message": e.to_string(), "type": e.code() }
                });
                match public {
                    PublicProtocol::OpenAi => {
                        yield Ok(Bytes::from(format!("data: {payload}\n\n")));
                    }
                    PublicProtocol::Anthropic => {
                        yield Ok(Bytes::from(format!("event: error\ndata: {payload}\n\n")));
                    }
                    PublicProtocol::Responses => {
                        for sse in responses_builder.fail(&e.to_string()) {
                            yield Ok(Bytes::from(sse));
                        }
                    }
                }
            }
            match public {
                PublicProtocol::OpenAi => yield Ok(Bytes::from(SSE_DONE)),
                PublicProtocol::Anthropic => {
                    // Anthropic 的收尾序列（content_block_stop → message_delta →
                    // message_stop）由 builder.finish() 统一发出。**必须调用它**：
                    // 否则客户端状态机会一直等待（表现为「答完了界面还在转圈」）。
                    for sse in anthropic_builder.finish() {
                        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(sse.to_sse()));
                    }
                }
                PublicProtocol::Responses => {
                    for sse in responses_builder.finish() {
                        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(sse));
                    }
                }
            }
            let cost_usd = estimate_cost_usd(
                &model_for_record,
                200,
                totals.input_tokens,
                totals.output_tokens,
                totals.cached_input_tokens,
            );
            let rec = RequestRecord {
                at_ms: now_ms(),
                account_id: account.slot.id.clone(),
                account_label: account.slot.label.clone(),
                model: model_for_record.clone(),
                protocol: protocol_name,
                client_protocol: public.as_str(),
                stream: true,
                status: 200,
                error_code: None,
                usage: totals,
                attempts: rotation.attempted(),
                ttft_ms,
                total_ms: now_ms() - started,
                cost_usd,
            };
            if let Some(observer) = &stream_state.observer {
                observer(rec);
            }
        };

        let mut response_headers = HeaderMap::new();
        response_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        response_headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        return (response_headers, Body::from_stream(output)).into_response();
    }

    // 跳出循环只可能是传输层错误（没拿到响应）
    let error = transport_error.expect("循环只在 transport_error 被赋值后 break");
    record(
        &state,
        &current,
        &model,
        protocol,
        public,
        stream,
        &error,
        &usage_totals,
        rotation.attempted(),
        ttft_ms,
        started,
    );
    error_response(&error, envelope)
}

/// 解析下一个候选账号（不含已试过的）。
fn next_account(state: &ProxyState, rotation: &Rotation) -> Option<ResolvedAccount> {
    let accounts = state.resolved();
    let rules = state.rules.read().unwrap_or_else(|e| e.into_inner());
    let preferred = state.preferred_id.read().unwrap_or_else(|e| e.into_inner());
    let pool = state.pool.lock().unwrap_or_else(|e| e.into_inner());
    pool.select(&accounts, "", &rules, preferred.as_deref(), now_ms())
        .filter(|a| !rotation.has_tried(&a.key))
        .cloned()
        .or_else(|| accounts.into_iter().find(|a| !rotation.has_tried(&a.key)))
}

/// 记录一次请求的结果。
#[allow(clippy::too_many_arguments)]
fn record(
    state: &ProxyState,
    account: &ResolvedAccount,
    model: &str,
    protocol: UpstreamProtocol,
    client_protocol: PublicProtocol,
    stream: bool,
    error: &CcError,
    usage: &UsageTotals,
    attempts: usize,
    ttft_ms: Option<i64>,
    started: i64,
) {
    let Some(observer) = &state.observer else {
        return;
    };
    let status = error.http_status();
    let cost_usd = estimate_cost_usd(
        model,
        status,
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
    );
    observer(RequestRecord {
        at_ms: now_ms(),
        account_id: account.slot.id.clone(),
        account_label: account.slot.label.clone(),
        model: model.to_string(),
        protocol: protocol_name(protocol),
        client_protocol: client_protocol.as_str(),
        stream,
        status,
        error_code: Some(error.code().to_string()),
        usage: *usage,
        attempts,
        ttft_ms,
        total_ms: now_ms() - started,
        cost_usd,
    });
}

/// 上游通道的稳定名字（用于记录与 UI）。
fn protocol_name(protocol: UpstreamProtocol) -> &'static str {
    match protocol {
        UpstreamProtocol::Cli => "cli",
        UpstreamProtocol::ProviderApi | UpstreamProtocol::Auto => "openai",
    }
}

/// 把事件累积到完整响应（非流式路径用）。
fn collect_event(
    event: &crate::sse::UpstreamEvent,
    content: &mut String,
    reasoning: &mut String,
    tool_calls: &mut Vec<Value>,
    finish: &mut String,
) {
    use crate::sse::UpstreamEvent as E;
    match event {
        E::TextDelta(t) => content.push_str(t),
        E::ReasoningDelta(t) => reasoning.push_str(t),
        E::ToolCall { id, name, input } => tool_calls.push(json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
            }
        })),
        E::FinishStep { finish_reason, .. } | E::Finish { finish_reason, .. } => {
            if let Some(r) = finish_reason {
                *finish = r.clone();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_names_are_stable() {
        assert_eq!(protocol_name(UpstreamProtocol::Cli), "cli");
        assert_eq!(protocol_name(UpstreamProtocol::ProviderApi), "openai");
        assert_eq!(protocol_name(UpstreamProtocol::Auto), "openai");
    }

    #[test]
    fn error_response_carries_retry_after_for_rate_limit() {
        let err = CcError::RateLimit {
            message: "exhausted".into(),
            retry_after_ms: Some(60_000),
        };
        let response = error_response(&err, ErrorEnvelope::OpenAi);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("60"),
            "Retry-After 应以秒为单位，让 SDK 拿到正确的退避提示"
        );
    }

    #[test]
    fn error_response_shape_differs_by_envelope() {
        let err = CcError::MissingCredential;
        let openai = error_response(&err, ErrorEnvelope::OpenAi);
        assert_eq!(openai.status(), StatusCode::UNAUTHORIZED);
        let anthropic = error_response(&err, ErrorEnvelope::Anthropic);
        assert_eq!(anthropic.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn collect_event_accumulates_content_and_tool_calls() {
        use crate::sse::UpstreamEvent as E;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = Vec::new();
        let mut finish = "stop".to_string();
        collect_event(
            &E::TextDelta("hi".into()),
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
        );
        collect_event(
            &E::ReasoningDelta("think".into()),
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
        );
        collect_event(
            &E::ToolCall {
                id: "c1".into(),
                name: "f".into(),
                input: json!({"a": 1}),
            },
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
        );
        collect_event(
            &E::Finish {
                finish_reason: Some("tool-calls".into()),
                usage: None,
            },
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
        );
        assert_eq!(content, "hi");
        assert_eq!(reasoning, "think");
        assert_eq!(tools.len(), 1);
        assert_eq!(finish, "tool-calls");
    }

    #[tokio::test]
    async fn health_reports_ok() {
        let state =
            Arc::new(ProxyState::new(Config::default(), Vec::new(), Arc::new(|_| None)).unwrap());
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let body = reqwest::get(format!("http://{addr}/health"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("\"status\":\"ok\""),
            "health 应返回 status ok，实际：{body}"
        );
    }

    #[tokio::test]
    async fn missing_credential_yields_401_in_openai_shape() {
        let state =
            Arc::new(ProxyState::new(Config::default(), Vec::new(), Arc::new(|_| None)).unwrap());
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&json!({"model": "m", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "invalid_api_key");
    }

    #[test]
    fn set_accounts_updates_slots_and_resolver() {
        let config = Config::default();
        let state = ProxyState::new(config, Vec::new(), Arc::new(|_| None)).unwrap();
        assert_eq!(state.slots.read().unwrap().len(), 0);

        let new_slots = vec![AccountSlot {
            id: "1".into(),
            label: "Acc1".into(),
        }];
        state.set_accounts(new_slots, Arc::new(|slot| Some(format!("key-{}", slot.id))));
        assert_eq!(state.slots.read().unwrap().len(), 1);
        let resolved = state.resolved();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].key, "key-1");
    }

    #[tokio::test]
    async fn alias_routes_are_bound_and_reachable() {
        let state =
            Arc::new(ProxyState::new(Config::default(), Vec::new(), Arc::new(|_| None)).unwrap());
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = reqwest::Client::new();

        // 验证 Anthropic 容错路径: /messages 与 /v1/v1/messages
        let r1 = client
            .post(format!("http://{addr}/messages"))
            .json(&json!({"model": "claude-3-opus", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r1.status().as_u16(),
            401,
            "/messages 路由应到达并返回 401（未配置 key）而非 404"
        );

        let r2 = client
            .post(format!("http://{addr}/v1/v1/messages"))
            .json(&json!({"model": "claude-3-opus", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r2.status().as_u16(),
            401,
            "/v1/v1/messages 容错路由应到达并返回 401 而非 404"
        );

        // 验证 OpenAI 容错路径: /chat/completions 与 /responses
        let r3 = client
            .post(format!("http://{addr}/chat/completions"))
            .json(&json!({"model": "gpt-4o", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r3.status().as_u16(),
            401,
            "/chat/completions 路由应到达并返回 401 而非 404"
        );

        let r4 = client
            .post(format!("http://{addr}/responses"))
            .json(&json!({"model": "gpt-4o", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r4.status().as_u16(),
            401,
            "/responses 路由应到达并返回 401 而非 404"
        );
    }

    // ---- 配额快照 → 账号池探测的换算 ----

    fn window(used: f64, cap: f64, exceeded: bool, reset_at_ms: i64) -> crate::quota::WindowUsage {
        crate::quota::WindowUsage {
            used,
            cap,
            exceeded,
            reset_at_ms,
        }
    }

    #[test]
    fn snapshot_without_windows_yields_no_probe() {
        // credits 端点失败或字段漂移：没有信息，必须返回 None 让调用方保留旧状态。
        // 若这里错误地返回「未超限」，一次失败的探测就会把账号误判为可用。
        let snapshot = crate::quota::QuotaSnapshot::default();
        assert!(window_probe_from_snapshot(&snapshot).is_none());
    }

    #[test]
    fn healthy_window_reports_not_exceeded() {
        let snapshot = crate::quota::QuotaSnapshot {
            five_hour: Some(window(1.0, 100.0, false, 0)),
            ..Default::default()
        };
        let probe = window_probe_from_snapshot(&snapshot).expect("有窗口数据就应有探测结果");
        assert!(!probe.exceeded, "未超限应能清除账号的 429 标记");
    }

    #[test]
    fn exceeded_window_picks_the_earliest_reset() {
        // 两个窗口都超限时取最早的：取晚了会让客户端睡过头，
        // 而 exhausted_error 要把它作为 Retry-After 回报。
        let snapshot = crate::quota::QuotaSnapshot {
            five_hour: Some(window(100.0, 100.0, true, 5_000)),
            weekly: Some(window(200.0, 200.0, true, 3_000)),
            ..Default::default()
        };
        let probe = window_probe_from_snapshot(&snapshot).expect("应产出探测结果");
        assert!(probe.exceeded);
        assert_eq!(probe.reset_at_ms, 3_000, "应取最早的重置时刻");
    }

    #[test]
    fn one_exceeded_window_is_enough_to_keep_the_account_out() {
        // 只有周窗口超限、5 小时窗口健康：账号仍不可用（任一窗口耗尽即不可服务）
        let snapshot = crate::quota::QuotaSnapshot {
            five_hour: Some(window(1.0, 100.0, false, 0)),
            weekly: Some(window(200.0, 200.0, true, 7_000)),
            ..Default::default()
        };
        let probe = window_probe_from_snapshot(&snapshot).expect("应产出探测结果");
        assert!(probe.exceeded, "任一窗口超限即不可用");
        assert_eq!(probe.reset_at_ms, 7_000);
    }
}
