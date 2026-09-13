//! 控制 API：供本应用的前端读取账号、流水与配额。
//!
//! **与代理面分开监听**：代理面（`/v1/*`）是给外部客户端用的，控制面
//! （`/api/*`）只给本应用的 WebView 用。分开的理由是鉴权模型不同——
//! 控制面用每次启动生成的随机 token 保护，而代理面要能被任意本地客户端直接调用。
//! 若两者同端口，任何能访问代理面的进程就能读账号列表（见 docs/ARCHITECTURE.md 第 7 节）。
//!
//! **token 校验**：所有 `/api/*` 路由都要求 `x-control-token`。token 由宿主生成
//! 并在启动时注入 WebView（`window.__CC_CONTROL__`），不落到磁盘、不写日志。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store::{NewRequest, Store};

/// 内存日志条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp_ms: i64,
    pub level: String,
    pub message: String,
}

/// 内存循环日志缓冲区（固定容量，默认 500 条）。
pub struct LogBuffer {
    counter: std::sync::atomic::AtomicU64,
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            counter: std::sync::atomic::AtomicU64::new(1),
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn push(&self, level: impl Into<String>, message: impl Into<String>) {
        let entry = LogEntry {
            id: self
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            timestamp_ms: crate::time::now_epoch_ms(),
            level: level.into(),
            message: message.into(),
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn push_raw(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let level = if trimmed.contains("ERROR") {
            "ERROR"
        } else if trimmed.contains("WARN") {
            "WARN"
        } else if trimmed.contains("DEBUG") {
            "DEBUG"
        } else {
            "INFO"
        };
        self.push(level, trimmed);
    }

    pub fn recent(&self, limit: usize) -> Vec<LogEntry> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let skip = entries.len().saturating_sub(limit);
        entries.iter().skip(skip).cloned().collect()
    }

    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.clear();
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(500)
    }
}

/// 账号变更回调类型。
pub type AccountChangeCallback = Arc<dyn Fn() + Send + Sync>;

/// 控制面共享状态。
pub struct ControlState {
    /// 存储句柄。
    pub store: Arc<Store>,
    /// 本次启动生成的随机 token。
    pub token: String,
    /// 上游基址。
    pub api_base: String,
    /// 账号变更回调（通知代理池与轮询器热重载）。
    pub on_account_changed: Option<AccountChangeCallback>,
    /// 运行日志循环缓冲区。
    pub log_buffer: Arc<LogBuffer>,
}

impl ControlState {
    /// 组装控制面状态。
    pub fn new(store: Arc<Store>, token: impl Into<String>) -> Self {
        Self {
            store,
            token: token.into(),
            api_base: "https://api.commandcode.ai".into(),
            on_account_changed: None,
            log_buffer: Arc::new(LogBuffer::default()),
        }
    }

    /// 设置上游基址。
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// 设置账号变更回调。
    pub fn with_account_callback(mut self, cb: AccountChangeCallback) -> Self {
        self.on_account_changed = Some(cb);
        self
    }

    /// 设置日志循环缓冲区。
    pub fn with_log_buffer(mut self, log_buffer: Arc<LogBuffer>) -> Self {
        self.log_buffer = log_buffer;
        self
    }

    /// 触发账号变更通知。
    pub fn notify_account_changed(&self) {
        if let Some(cb) = &self.on_account_changed {
            cb();
        }
    }
}

/// 账号的对外形状（**绝不包含密文**，只给提示）。
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    /// 主键。
    pub id: i64,
    /// 展示名。
    pub label: String,
    /// 密钥提示，形如 `user_…ab12`。
    pub key_hint: String,
    /// 是否启用。
    pub enabled: bool,
    /// 最近一次配额快照（原样透传，前端自行解析）。
    pub quota: serde_json::Value,
    /// 最近一次错误。
    pub last_error: Option<String>,
    /// 最近一次刷新时刻。
    pub last_checked_ms: Option<i64>,
}

/// 请求流水的对外形状。
#[derive(Debug, Clone, Serialize)]
pub struct RequestView {
    /// 主键。
    pub id: i64,
    /// 时刻。
    pub at_ms: i64,
    /// 账号 id。
    pub account_id: String,
    /// 账号展示名（备注）。
    pub account_label: Option<String>,
    /// 模型。
    pub model: String,
    /// 上游通道。
    pub protocol: String,
    /// 客户端协议。
    pub client_protocol: String,
    /// 是否流式。
    pub stream: bool,
    /// 状态码。
    pub status: u16,
    /// 错误码。
    pub error_code: Option<String>,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 缓存命中。
    pub cached_tokens: i64,
    /// 首字节耗时。
    pub ttft_ms: Option<i64>,
    /// 总耗时。
    pub total_ms: i64,
    /// 预估消耗金额（美元）。
    pub cost_usd: f64,
}

/// 控制面错误信封。
#[derive(Debug, Serialize)]
struct ControlError {
    error: String,
}

/// 构造控制面路由。`token` 为空时**不启用**鉴权（仅供单元测试）。
///
/// # 为什么需要 CORS
///
/// 生产构建下 WebView 的页面源是 `tauri://localhost`（macOS）或
/// `http://tauri.localhost`（Windows），而控制面跑在 `http://127.0.0.1:<随机端口>`。
/// 两者**不同源**，浏览器会强制 CORS：缺少响应头时 `fetch` 直接以
/// `TypeError: Load failed` 失败（连状态码都拿不到），界面表现为「无法连接本地服务」。
///
/// 允许任意源是安全的，理由：
/// - token 仍是必需项（见 [require_token]），别的网页拿不到它；
/// - 端口每次启动随机；
/// - 服务只监听回环地址。
///
/// 因此 CORS 在这里是「让本应用自己的页面能访问」，不是权限边界。
pub fn control_router(state: Arc<ControlState>) -> Router {
    use tower_http::cors::{Any, CorsLayer};

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        // 自定义头 x-control-token 必须显式放行，否则预检请求不通过
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderName::from_static("x-control-token"),
        ]);

    Router::new()
        .route("/api/health", get(health))
        .route("/health", get(health))
        .route("/api/accounts/list", post(list_accounts))
        .route("/api/accounts/refresh", post(refresh_accounts))
        .route("/api/accounts", post(create_account))
        .route("/api/accounts/{id}", patch(update_account))
        .route("/api/accounts/{id}", axum::routing::delete(delete_account))
        .route("/api/requests/recent", post(recent_requests))
        .route("/api/requests/insert", post(insert_request))
        .route("/api/rules", get(list_rules).put(update_rules))
        .route("/api/settings", get(get_all_settings))
        .route("/api/settings/{key}", get(get_setting).put(set_setting))
        .route("/api/logs", get(get_logs))
        .route("/api/logs/clear", post(clear_logs))
        // 路由层鉴权
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        // CORS 层在最外层包裹整个服务；预检请求由 CORS 层放行
        .layer(cors)
        .with_state(state)
}

/// 校验 `x-control-token`。
async fn require_token(
    State(state): State<Arc<ControlState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // 测试路径或 CORS 预检请求（OPTIONS）：不校验 token，交给 CORS 层处理
    if state.token.is_empty() || request.method() == axum::http::Method::OPTIONS {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get("x-control-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // 定长比较：避免按字节提前返回而泄露 token 前缀
    if !constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(ControlError {
                error: "控制面令牌无效".into(),
            }),
        )
            .into_response();
    }
    next.run(request).await
}

/// 定长字节比较（长度不同直接返回 false，长度本身不是秘密）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 健康检查。
async fn health(State(state): State<Arc<ControlState>>) -> Response {
    let count = state.store.list_accounts().map(|a| a.len()).unwrap_or(0);
    axum::Json(json!({
        "status": "ok",
        "accounts": count,
        "api_base": state.api_base,
    }))
    .into_response()
}

/// 账号列表。
async fn list_accounts(State(state): State<Arc<ControlState>>) -> Response {
    match state.store.list_accounts() {
        Ok(rows) => {
            let accounts: Vec<AccountView> = rows.into_iter().map(to_account_view).collect();
            axum::Json(json!({ "accounts": accounts })).into_response()
        }
        Err(e) => control_error(e),
    }
}

/// 添加账号。
///
/// **接收的是密文**：加解密在宿主层完成，控制面只负责落库。这样即使
/// 前端被注入脚本，也无法通过这个端点拿到明文密钥（它本来就没有）。
#[derive(Debug, Deserialize)]
struct CreateAccountBody {
    label: String,
    /// 密文（base64 或原始字节数组，由宿主决定编码）。
    key_cipher: Vec<u8>,
    key_hint: String,
    created_at_ms: i64,
}

async fn create_account(
    State(state): State<Arc<ControlState>>,
    axum::Json(body): axum::Json<CreateAccountBody>,
) -> Response {
    if body.label.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(ControlError {
                error: "账号名称不能为空".into(),
            }),
        )
            .into_response();
    }
    if body.key_cipher.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(ControlError {
                error: "密钥不能为空".into(),
            }),
        )
            .into_response();
    }
    // 重复检测按提示（同一 key 的提示相同）
    match state.store.find_account_by_hint(&body.key_hint) {
        Ok(Some(existing)) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "error": format!("该密钥已存在（{}）", existing.label),
                })),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(e) => return control_error(e),
    }
    match state.store.insert_account(
        body.label.trim(),
        &body.key_cipher,
        &body.key_hint,
        body.created_at_ms,
    ) {
        Ok(id) => {
            state.notify_account_changed();
            axum::Json(json!({ "id": id })).into_response()
        }
        Err(e) => control_error(e),
    }
}

/// 触发账号配额立即轮询刷新。
async fn refresh_accounts(State(state): State<Arc<ControlState>>) -> Response {
    state.notify_account_changed();
    axum::Json(json!({ "ok": true })).into_response()
}

/// 修改账号（重命名 / 启停）。
#[derive(Debug, Deserialize)]
struct UpdateAccountBody {
    label: Option<String>,
    enabled: Option<bool>,
}

async fn update_account(
    State(state): State<Arc<ControlState>>,
    Path(id): Path<i64>,
    axum::Json(body): axum::Json<UpdateAccountBody>,
) -> Response {
    let mut changed = false;
    if let Some(label) = body.label.as_deref() {
        if label.trim().is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(ControlError {
                    error: "账号名称不能为空".into(),
                }),
            )
                .into_response();
        }
        match state.store.rename_account(id, label.trim()) {
            Ok(ok) => changed |= ok,
            Err(e) => return control_error(e),
        }
    }
    if let Some(enabled) = body.enabled {
        match state.store.set_account_enabled(id, enabled) {
            Ok(ok) => changed |= ok,
            Err(e) => return control_error(e),
        }
    }
    if !changed {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(ControlError {
                error: "账号不存在或没有需要修改的字段".into(),
            }),
        )
            .into_response();
    }
    state.notify_account_changed();
    axum::Json(json!({ "ok": true })).into_response()
}

/// 删除账号。
async fn delete_account(State(state): State<Arc<ControlState>>, Path(id): Path<i64>) -> Response {
    match state.store.delete_account(id) {
        Ok(true) => {
            state.notify_account_changed();
            axum::Json(json!({ "ok": true })).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            axum::Json(ControlError {
                error: "账号不存在".into(),
            }),
        )
            .into_response(),
        Err(e) => control_error(e),
    }
}

/// 最近请求流水。
#[derive(Debug, Deserialize)]
struct RecentRequestsBody {
    #[serde(default = "default_limit")]
    limit: i64,
}

/// 默认返回条数。
fn default_limit() -> i64 {
    50
}

async fn recent_requests(
    State(state): State<Arc<ControlState>>,
    axum::Json(body): axum::Json<RecentRequestsBody>,
) -> Response {
    // 上限保护：面板不该一次拉几万行
    let limit = body.limit.clamp(1, 500);
    match state.store.recent_requests(limit) {
        Ok(rows) => {
            let requests: Vec<RequestView> = rows.into_iter().map(to_request_view).collect();
            axum::Json(json!({ "requests": requests })).into_response()
        }
        Err(e) => control_error(e),
    }
}

/// 代宿主写入一条请求流水。
///
/// 代理进程与 UI 可能分属不同进程/连接，因此把「落库」也做成一个受保护的端点，
/// 让代理侧可以只把记录 POST 过来。
async fn insert_request(
    State(state): State<Arc<ControlState>>,
    axum::Json(body): axum::Json<NewRequest>,
) -> Response {
    match state.store.insert_request(&body) {
        Ok(id) => axum::Json(json!({ "id": id })).into_response(),
        Err(e) => control_error(e),
    }
}

/// 读一个设置项。
async fn get_setting(State(state): State<Arc<ControlState>>, Path(key): Path<String>) -> Response {
    match state.store.get_setting(&key) {
        Ok(Some(value)) => axum::Json(json!({ "key": key, "value": value })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(ControlError {
                error: "设置不存在".into(),
            }),
        )
            .into_response(),
        Err(e) => control_error(e),
    }
}

/// 写一个设置项。
#[derive(Debug, Deserialize)]
struct SetSettingBody {
    value: String,
}

async fn set_setting(
    State(state): State<Arc<ControlState>>,
    Path(key): Path<String>,
    axum::Json(body): axum::Json<SetSettingBody>,
) -> Response {
    match state.store.set_setting(&key, &body.value) {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => control_error(e),
    }
}

/// 路由规则视图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRuleView {
    pub id: i64,
    pub models: Vec<String>,
    pub account_id: String,
}

async fn list_rules(State(state): State<Arc<ControlState>>) -> Response {
    match state.store.list_route_rules() {
        Ok(rows) => {
            let rules: Vec<RouteRuleView> = rows
                .into_iter()
                .map(|r| {
                    let models =
                        serde_json::from_str::<Vec<String>>(&r.models_json).unwrap_or_default();
                    RouteRuleView {
                        id: r.id,
                        models,
                        account_id: r.account_id,
                    }
                })
                .collect();
            axum::Json(json!({ "rules": rules })).into_response()
        }
        Err(e) => control_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateRulesBody {
    rules: Vec<UpdateRuleItem>,
}

#[derive(Debug, Deserialize)]
struct UpdateRuleItem {
    models: Vec<String>,
    account_id: String,
}

async fn update_rules(
    State(state): State<Arc<ControlState>>,
    axum::Json(body): axum::Json<UpdateRulesBody>,
) -> Response {
    let pairs: Vec<(String, String)> = body
        .rules
        .into_iter()
        .map(|item| {
            let models_json = serde_json::to_string(&item.models).unwrap_or_else(|_| "[]".into());
            (models_json, item.account_id)
        })
        .collect();

    match state.store.replace_route_rules(&pairs) {
        Ok(()) => {
            state.notify_account_changed();
            axum::Json(json!({ "ok": true })).into_response()
        }
        Err(e) => control_error(e),
    }
}

async fn get_all_settings(State(state): State<Arc<ControlState>>) -> Response {
    let retention = state
        .store
        .get_setting("retention")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(5000);
    axum::Json(json!({
        "retention": retention,
        "api_base": state.api_base,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct GetLogsQuery {
    limit: Option<usize>,
}

async fn get_logs(
    State(state): State<Arc<ControlState>>,
    Query(query): Query<GetLogsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let logs = state.log_buffer.recent(limit);
    axum::Json(json!({ "logs": logs })).into_response()
}

async fn clear_logs(State(state): State<Arc<ControlState>>) -> Response {
    state.log_buffer.clear();
    axum::Json(json!({ "ok": true })).into_response()
}

/// 行 → 对外形状。密文**不出现**在这个结构里。
fn to_account_view(row: crate::store::AccountRow) -> AccountView {
    let quota = row
        .quota_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    AccountView {
        id: row.id,
        label: row.label,
        key_hint: row.key_hint,
        enabled: row.enabled,
        quota,
        last_error: row.last_error,
        last_checked_ms: row.last_checked_ms,
    }
}

/// 行 → 对外形状。
fn to_request_view(row: crate::store::RequestRow) -> RequestView {
    RequestView {
        id: row.id,
        at_ms: row.at_ms,
        account_id: row.account_id,
        account_label: row.account_label,
        model: row.model,
        protocol: row.protocol,
        client_protocol: row.client_protocol,
        stream: row.stream,
        status: row.status,
        error_code: row.error_code,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cached_tokens: row.cached_tokens,
        ttft_ms: row.ttft_ms,
        total_ms: row.total_ms,
        cost_usd: row.cost_usd,
    }
}

/// 把存储错误渲染成 500 + JSON 信封。
fn control_error(error: crate::error::CcError) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(ControlError {
            error: error.to_string(),
        }),
    )
        .into_response()
}

/// 便于测试：从请求头里取 token 的辅助（与中间件同一套规则）。
pub fn token_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-control-token").and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state() -> Arc<ControlState> {
        Arc::new(ControlState::new(
            Arc::new(Store::open_in_memory().unwrap()),
            "secret-token",
        ))
    }

    async fn call(
        state: Arc<ControlState>,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let app = control_router(state);
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    fn with_token(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header("x-control-token", "secret-token")
            .header("content-type", "application/json")
    }

    #[tokio::test]
    async fn missing_token_is_rejected() {
        let app = control_router(state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/accounts/list")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "控制面必须校验 token"
        );
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let app = control_router(state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/accounts/list")
                    .header("x-control-token", "wrong")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn account_lifecycle_over_the_control_api() {
        let st = state();
        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("POST").uri("/api/accounts"))
                .body(Body::from(r#"{"label":"Go #1","key_cipher":[1,2,3],"key_hint":"user_…ab12","created_at_ms":100}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_i64().unwrap();

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("POST").uri("/api/accounts/list"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["accounts"][0]["label"], "Go #1");
        // 对外形状绝不能包含密文
        assert!(
            body["accounts"][0].get("key_cipher").is_none(),
            "响应不得暴露密文"
        );

        let (status, _) = call(
            st.clone(),
            with_token(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/accounts/{id}")),
            )
            .body(Body::from(r#"{"enabled":false}"#))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(
            st.clone(),
            with_token(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/accounts/{id}")),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn duplicate_key_is_conflict() {
        let st = state();
        let create = || {
            with_token(Request::builder().method("POST").uri("/api/accounts"))
                .body(Body::from(
                    r#"{"label":"a","key_cipher":[1],"key_hint":"same","created_at_ms":1}"#,
                ))
                .unwrap()
        };
        let (status, _) = call(st.clone(), create()).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(st.clone(), create()).await;
        assert_eq!(status, StatusCode::CONFLICT, "同一密钥不应被重复添加");
        assert!(body["error"].as_str().unwrap().contains("已存在"));
    }

    #[tokio::test]
    async fn empty_label_or_key_is_rejected() {
        let st = state();
        let (status, _) = call(
            st.clone(),
            with_token(Request::builder().method("POST").uri("/api/accounts"))
                .body(Body::from(
                    r#"{"label":"  ","key_cipher":[1],"key_hint":"h","created_at_ms":1}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = call(
            st.clone(),
            with_token(Request::builder().method("POST").uri("/api/accounts"))
                .body(Body::from(
                    r#"{"label":"a","key_cipher":[],"key_hint":"h","created_at_ms":1}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "空密钥应被拒绝");
    }

    #[tokio::test]
    async fn requests_are_returned_newest_first_and_capped() {
        let st = state();
        for i in 0..3 {
            let body = format!(
                r#"{{"at_ms":{},"account_id":"a","model":"m","protocol":"cli","stream":true,"status":200,"error_code":null,"input_tokens":1,"output_tokens":1,"cached_tokens":0,"ttft_ms":null,"total_ms":1,"attempts":1}}"#,
                1000 + i
            );
            let (status, _) = call(
                st.clone(),
                with_token(
                    Request::builder()
                        .method("POST")
                        .uri("/api/requests/insert"),
                )
                .body(Body::from(body))
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, body) = call(
            st.clone(),
            with_token(
                Request::builder()
                    .method("POST")
                    .uri("/api/requests/recent"),
            )
            .body(Body::from(r#"{"limit":2}"#))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["requests"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "limit 应被尊重");
        assert_eq!(rows[0]["at_ms"], 1002, "最新的排在最前");
    }

    #[tokio::test]
    async fn settings_roundtrip_over_the_api() {
        let st = state();
        let (status, _) = call(
            st.clone(),
            with_token(Request::builder().method("PUT").uri("/api/settings/port"))
                .body(Body::from(r#"{"value":"3050"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/api/settings/port"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], "3050");
    }

    #[tokio::test]
    async fn cors_preflight_for_accounts_list_passes() {
        let app = control_router(state());
        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("OPTIONS")
                .uri("/api/accounts/list")
                .header("origin", "tauri://localhost")
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "content-type, x-control-token",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn health_endpoints_return_api_base() {
        let st = state();
        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/api/health"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["api_base"], "https://api.commandcode.ai");

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/health"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["api_base"], "https://api.commandcode.ai");
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn rules_list_and_update_over_api() {
        let st = state();
        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/api/rules"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rules"].as_array().unwrap().len(), 0);

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("PUT").uri("/api/rules"))
                .body(Body::from(
                    r#"{"rules":[{"models":["claude-*"],"account_id":"acc-1"}]}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/api/rules"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rules = body["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["account_id"], "acc-1");
        assert_eq!(rules[0]["models"][0], "claude-*");
    }

    #[tokio::test]
    async fn logs_buffer_and_api() {
        let st = state();
        st.log_buffer.push("INFO", "test message 1");
        st.log_buffer
            .push_raw("2026-09-13 [WARN] high latency detected");

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/api/logs"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let logs = body["logs"].as_array().unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0]["message"], "test message 1");
        assert_eq!(logs[1]["level"], "WARN");

        let (status, body) = call(
            st.clone(),
            with_token(Request::builder().method("POST").uri("/api/logs/clear"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);

        let (_, body) = call(
            st.clone(),
            with_token(Request::builder().method("GET").uri("/api/logs"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(body["logs"].as_array().unwrap().len(), 0);
    }
}
