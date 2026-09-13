//! 上游 HTTP 客户端：伪装头、会话与设备指纹、请求发送。
//!
//! 对应 third_party/proxy.mjs 的 forwardToCC / ensureSession / ensureInitialized
//! 与 generateFingerprint。移植时保留了全部伪装细节——上游会校验这些头。
//!
//! 三个概念必须区分清楚（这是本模块最容易混淆的地方）：
//! - **session id**：按 key 稳定的会话标识，12h + 抖动后更换；
//! - **thread id**：**每次请求**新生成，标识一次生成；
//! - **fingerprint**：按 key 生成的设备指纹，"随机但稳定"，8h + 抖动后重录。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{Config, UpstreamProtocol};
use crate::error::CcError;
use crate::sse::{LineBuffer, UpstreamEvent};

/// session 有效期。
const SESSION_DURATION_MS: u64 = 12 * 60 * 60 * 1000;
/// session 抖动范围：避免所有客户端同时换 session。
const SESSION_JITTER_MS: u64 = 60 * 60 * 1000;
/// 指纹重录间隔。
const INIT_REFRESH_MS: u64 = 8 * 60 * 60 * 1000;
/// 指纹重录抖动范围。
const INIT_JITTER_MS: u64 = 2 * 60 * 60 * 1000;

/// 当前时间的 epoch 毫秒。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 一个 key 的会话状态。
#[derive(Debug, Clone)]
struct KeyState {
    session_id: String,
    session_expires_at_ms: u64,
    fingerprint: Value,
    next_init_at_ms: u64,
}

/// 上游客户端：持有每个 key 的会话与指纹。
#[derive(Debug)]
pub struct UpstreamClient {
    http: reqwest::Client,
    config: Config,
    /// key → 会话状态。无界增长的担忧是多余的：条目数等于账号数。
    keys: Mutex<HashMap<String, KeyState>>,
    /// key → 已学到的协议偏好（true 表示该 key 只能走 CLI 通道）。
    protocol: Mutex<HashMap<String, (bool, u64)>>,
}

/// 学到的协议偏好缓存时长。
///
/// 到期后允许重新探测 Provider API，这样**升级到 Pro 的账号无需重启**即可恢复。
const PROTOCOL_CACHE_TTL_MS: u64 = 15 * 60 * 1000;

impl UpstreamClient {
    /// 用配置构造客户端。
    pub fn new(config: Config) -> Result<Self, CcError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| CcError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            config,
            keys: Mutex::new(HashMap::new()),
            protocol: Mutex::new(HashMap::new()),
        })
    }

    /// 当前配置。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 取（必要时创建）某个 key 的会话状态。
    fn with_key_state<T>(&self, key: &str, f: impl FnOnce(&mut KeyState) -> T) -> T {
        let now = now_ms() as u64;
        let mut map = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let state = map.entry(key.to_string()).or_insert_with(|| {
            let mut rng = rand::rng();
            let jitter = rng.random_range(0..SESSION_JITTER_MS);
            KeyState {
                session_id: uuid::Uuid::new_v4().to_string(),
                session_expires_at_ms: now + SESSION_DURATION_MS + jitter,
                fingerprint: generate_fingerprint(),
                next_init_at_ms: 0,
            }
        });
        // 会话过期则换新（保持「同一 key 在同一周期内复用」的语义）
        if now >= state.session_expires_at_ms {
            let mut rng = rand::rng();
            let jitter = rng.random_range(0..SESSION_JITTER_MS);
            state.session_id = uuid::Uuid::new_v4().to_string();
            state.session_expires_at_ms = now + SESSION_DURATION_MS + jitter;
            state.fingerprint = generate_fingerprint();
            state.next_init_at_ms = 0;
        }
        f(state)
    }

    /// 该 key 当前应使用的 session id。
    pub fn session_id(&self, key: &str) -> String {
        self.with_key_state(key, |s| s.session_id.clone())
    }

    /// 采用客户端自带的会话 id（若与当前不同）。
    ///
    /// 上游按 session 组织前缀缓存：客户端（如 Claude Code）已经在维护会话边界时，
    /// 透传它的 id 比我们自行生成更准确——同一会话的请求会稳定落进同一缓存分组。
    /// 同时把过期时间顺延一个完整周期，避免客户端会话还在继续而我们的 id 先失效。
    pub fn adopt_session(&self, key: &str, session_id: &str) {
        let now = now_ms() as u64;
        self.with_key_state(key, |s| {
            if s.session_id != session_id {
                s.session_id = session_id.to_string();
            }
            let mut rng = rand::rng();
            s.session_expires_at_ms =
                now + SESSION_DURATION_MS + rng.random_range(0..SESSION_JITTER_MS);
        });
    }

    /// 该 key 的指纹是否已过期（需要重新录制）。
    fn fingerprint_due(&self, key: &str) -> Option<Value> {
        let now = now_ms() as u64;
        self.with_key_state(key, |s| {
            if now < s.next_init_at_ms {
                return None;
            }
            let mut rng = rand::rng();
            s.next_init_at_ms = now + INIT_REFRESH_MS + rng.random_range(0..INIT_JITTER_MS);
            Some(s.fingerprint.clone())
        })
    }

    /// 构造 /alpha/generate 的请求头。
    ///
    /// 这些头**全部是伪装所需**，删任何一个都可能导致上游拒绝。见 docs/PROTOCOL.md 第 2 节。
    pub fn generate_headers(&self, key: &str) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut headers = HeaderMap::new();
        let mut put = |name: &str, value: &str| {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        };
        put("Content-Type", "application/json");
        put("Authorization", &format!("Bearer {key}"));
        put("x-cli-environment", "production");
        put("x-command-code-version", &self.config.cli_version);
        put("x-session-id", &self.session_id(key));
        put("x-co-flag", "false");
        put("x-taste-learning", "false");
        put("x-project-slug", &self.config.project_slug);
        put("traceparent", &generate_traceparent());
        if self.config.zdr {
            put("x-cmd-zdr", "1");
        }
        headers
    }

    /// 该 key 的初始协议选择。
    ///
    /// 优先读协议缓存；否则由配置决定（Auto → Provider API）。
    pub fn initial_protocol(&self, key: &str) -> UpstreamProtocol {
        match self.config.upstream_protocol {
            UpstreamProtocol::Cli => return UpstreamProtocol::Cli,
            UpstreamProtocol::ProviderApi => return UpstreamProtocol::ProviderApi,
            UpstreamProtocol::Auto => {}
        }
        let now = now_ms() as u64;
        let map = self.protocol.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            Some((true, at)) if now - at < PROTOCOL_CACHE_TTL_MS => UpstreamProtocol::Cli,
            Some((false, at)) if now - at < PROTOCOL_CACHE_TTL_MS => UpstreamProtocol::ProviderApi,
            // 未知账号默认 CLI：我们只构造 CLI 形状的 body。
            // 改成 ProviderApi 需要同时补一个 Provider 形状的构造器，
            // 否则上游会报 `param: model` expected string, received undefined。
            _ => UpstreamProtocol::Cli,
        }
    }

    /// 记住「这个 key 只能走 CLI 通道」（因为它返回过 upgrade_required）。
    pub fn remember_cli_only(&self, key: &str) {
        let mut map = self.protocol.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(key.to_string(), (true, now_ms() as u64));
    }

    /// 构造上游端点 URL。
    pub fn endpoint(&self, protocol: UpstreamProtocol) -> String {
        let base = self.config.api_base.trim_end_matches('/');
        match protocol {
            // Auto 等价于 CLI：我们只构造 CLI 形状的请求体（见 UpstreamProtocol 文档）
            UpstreamProtocol::Cli | UpstreamProtocol::Auto => format!("{base}/alpha/generate"),
            UpstreamProtocol::ProviderApi => format!("{base}/provider/v1/chat/completions"),
        }
    }

    /// 发送一次生成请求，返回原始的响应（**尚未读取 body**）。
    ///
    /// 连接阶段受 \`request_timeout\` 约束；body 流由调用方用空闲超时看门狗管理——
    /// 用超时约束整个 body 会误杀长思考的健康请求（docs/PROTOCOL.md #4）。
    pub async fn send_generate(
        &self,
        key: &str,
        protocol: UpstreamProtocol,
        body: &Value,
    ) -> Result<reqwest::Response, CcError> {
        let url = self.endpoint(protocol);
        let request = if matches!(protocol, UpstreamProtocol::Cli) {
            self.http
                .post(&url)
                .headers(self.generate_headers(key))
                .json(body)
        } else {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|e| CcError::Protocol(e.to_string()))?,
            );
            headers.insert(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("text/event-stream"),
            );
            // 刻意**不**带 x-command-code-version / x-cli-environment：
            // 这是文档化的 OpenAI 兼容面，不是 CLI 通道，别"顺手补上"。
            self.http.post(&url).headers(headers).json(body)
        };
        let response = tokio::time::timeout(self.config.request_timeout, request.send())
            .await
            .map_err(|_| CcError::Timeout(self.config.request_timeout.as_millis() as u64))?
            .map_err(|e| CcError::Transport(format!("{e}")))?;
        Ok(response)
    }

    /// 把非 2xx 响应转成带语义的错误。
    ///
    /// 上游把多种业务拒绝折叠进 403（套餐超限、模型不可用、CLI 版本过低），
    /// 因此必须优先解析 \`error.code\`——只看状态码无法区分它们。
    pub async fn classify_error(response: reqwest::Response) -> CcError {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let code = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        // 只保留前 500 字符：上游偶发返回很长的 HTML 错误页
        let body = body.chars().take(500).collect();
        CcError::UpstreamHttp { status, code, body }
    }

    /// 记录设备指纹（首次与每 8h+抖动一次）。
    ///
    /// 这是**尽力而为**：失败只记日志，绝不影响正常对话——预请求的意义在于
    /// 让上游看到一致的行为模式，而不是承担任何功能。
    pub async fn ensure_initialized(&self, key: &str) {
        let Some(fingerprint) = self.fingerprint_due(key) else {
            return;
        };
        let base = self.config.api_base.trim_end_matches('/');
        for (path, payload) in [
            ("/alpha/fingerprint/record", fingerprint),
            (
                "/alpha/lifecycle-events",
                json!({ "events": [{ "type": "session-start", "timestamp": now_ms() }] }),
            ),
        ] {
            let url = format!("{base}{path}");
            let result = self
                .http
                .post(&url)
                .headers(self.generate_headers(key))
                .json(&payload)
                .send()
                .await;
            match result {
                Ok(r) if r.status().is_success() => {
                    tracing::debug!(path, "上游预请求成功");
                }
                Ok(r) => {
                    tracing::warn!(
                        path,
                        status = r.status().as_u16(),
                        "上游预请求被拒（不影响对话）"
                    );
                }
                Err(e) => {
                    tracing::warn!(path, error = %e, "上游预请求失败（不影响对话）");
                }
            }
        }
    }

    /// 账户端点（/alpha/whoami、/alpha/billing/* 等）使用的请求头。
    ///
    /// **与生成通道的头集有意不同**：生成通道需要完整的伪装（session id、
    /// traceparent、project slug、taste-learning 等，见 [Self::generate_headers]），
    /// 而账户端点是普通的只读查询接口，给它们塞生成通道的会话头既无意义，
    /// 也会让一个探测请求看起来像一次生成。
    ///
    /// 依据：MIT 上游 dsh-commandcode-provider 的 accountHeaders（生产在用、
    /// 214 stars）只带 Authorization + 版本 + 环境三项；本地固化副本见
    /// third_party/dsh-accounts.ts 所在仓库的 src/adapter.ts:1621。
    fn account_headers(&self, key: &str) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut headers = HeaderMap::new();
        let mut put = |name: &str, value: &str| {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        };
        put("Authorization", &format!("Bearer {key}"));
        put("Accept", "application/json");
        put("x-command-code-version", &self.config.cli_version);
        put("x-cli-environment", "production");
        headers
    }

    /// GET 一个账户端点并解析 JSON。
    ///
    /// 配额轮询与账户信息查询都走这里。**不直接暴露 reqwest::Client**：
    /// 请求头、超时、错误分类这三条规则必须只有一处实现，否则调用方会各写一遍
    /// 并逐渐与主路径分叉。path 必须以 / 开头。
    pub async fn get_account_json(&self, key: &str, path: &str) -> Result<Value, CcError> {
        debug_assert!(path.starts_with('/'), "账户端点的 path 必须以 / 开头");
        let url = format!("{}{}", self.config.api_base.trim_end_matches('/'), path);
        let response = self
            .http
            .get(&url)
            .headers(self.account_headers(key))
            // 账户端点必须快速失败：一个挂住的上游不能拖死配额轮询
            .timeout(Duration::from_millis(crate::config::MODELS_TIMEOUT_MS))
            .send()
            .await
            .map_err(|e| CcError::Transport(format!("{e}")))?;
        if !response.status().is_success() {
            return Err(Self::classify_error(response).await);
        }
        response
            .json()
            .await
            .map_err(|e| CcError::Protocol(e.to_string()))
    }

    /// 拉取模型目录。
    pub async fn list_models(&self, key: &str) -> Result<Value, CcError> {
        let url = format!(
            "{}/provider/v1/models",
            self.config.api_base.trim_end_matches('/')
        );
        let response = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
            .timeout(Duration::from_millis(crate::config::MODELS_TIMEOUT_MS))
            .send()
            .await
            .map_err(|e| CcError::Transport(format!("{e}")))?;
        if !response.status().is_success() {
            return Err(Self::classify_error(response).await);
        }
        response
            .json()
            .await
            .map_err(|e| CcError::Protocol(e.to_string()))
    }
}

/// 生成一个"随机但稳定"的设备指纹。
///
/// 与上游实现一致：所有字段都是**随机哈希**，平台信息写死为 win32/x64，
/// 与运行本程序的真实机器无关——目的是让上游看到一致且不泄露宿主信息的模式。
pub fn generate_fingerprint() -> Value {
    const CPUS: &[(&str, u32)] = &[
        ("Intel(R) Core(TM) i7-10700 CPU @ 2.90GHz", 16),
        ("AMD Ryzen 7 5800X 8-Core Processor", 16),
        ("Intel(R) Core(TM) i5-9400F CPU @ 2.90GHz", 6),
    ];
    const MEMS: &[u32] = &[8, 16, 32, 64];
    const TZS: &[&str] = &[
        "Asia/Shanghai",
        "Asia/Tokyo",
        "America/New_York",
        "Europe/London",
    ];

    let mut rng = rand::rng();
    let sha256 = |s: &str| {
        let mut hasher = Sha256::new();
        hasher.update(s.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    // 先取所有直接随机值，再建闭包：rand_hex 会可变借用 rng，二者不能同时存在
    let (cpu_model, cpu_cores) = CPUS[rng.random_range(0..CPUS.len())];
    let mem_gib = MEMS[rng.random_range(0..MEMS.len())];
    let tz = TZS[rng.random_range(0..TZS.len())];
    let mac_count = rng.random_range(1..=3);

    /// 生成指定字节数的随机十六进制串。
    fn rand_hex(rng: &mut impl Rng, bytes: usize) -> String {
        let v: Vec<u8> = (0..bytes).map(|_| rng.random::<u8>()).collect();
        v.iter().map(|b| format!("{b:02x}")).collect::<String>()
    }

    let mac_hashes: Vec<String> = (0..mac_count)
        .map(|_| sha256(&rand_hex(&mut rng, 32)))
        .collect();
    let machine_id_hash = sha256(&rand_hex(&mut rng, 32));
    let os_user_hash = sha256(&rand_hex(&mut rng, 16));
    let hostname_hash = sha256(&rand_hex(&mut rng, 16));
    let git_email_hash = sha256(&rand_hex(&mut rng, 16));

    let thumb_data = format!(
        "{}|{}|{}|{}|{}|win32|10.0.22631|{cpu_model}|{cpu_cores}|{mem_gib}",
        machine_id_hash,
        mac_hashes.join("|"),
        os_user_hash,
        hostname_hash,
        git_email_hash,
    );
    let thumbmark = sha256(&thumb_data);

    json!({
        "thumbmark": thumbmark,
        "components": {
            "machineIdHash": machine_id_hash,
            "macHashes": mac_hashes,
            "osUserHash": os_user_hash,
            "hostnameHash": hostname_hash,
            "gitEmailHash": git_email_hash,
            "platform": "win32",
            "arch": "x64",
            "osRelease": "10.0.22631",
            "cpuModel": cpu_model,
            "cpuCount": cpu_cores,
            "memGiB": mem_gib,
            "isContainer": false,
            "timezone": tz,
            "runtime": "cli",
            "collectorVersion": 1,
        }
    })
}

/// 生成一个 W3C traceparent 头。
fn generate_traceparent() -> String {
    let mut rng = rand::rng();
    let hex = |rng: &mut rand::rngs::ThreadRng, bytes: usize| {
        let v: Vec<u8> = (0..bytes).map(|_| rng.random::<u8>()).collect();
        v.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let trace_id = hex(&mut rng, 16);
    let span_id = hex(&mut rng, 8);
    format!("00-{trace_id}-{span_id}-01")
}

/// 把一个上游响应体转成事件流。
///
/// 上游两种承载都支持：\`/alpha/generate\` 是 NDJSON，\`/provider/v1\` 是 SSE。
/// 空闲超时**只计读取等待**并且每收到一个 chunk 重置——上游在两次输出之间可能
/// 长时间静默（推理模型思考），用固定超时约束整条流会误杀健康请求。
pub struct EventStream {
    response: reqwest::Response,
    buffer: LineBuffer,
    /// 已切分但尚未消费的完整行。
    pending_lines: std::collections::VecDeque<String>,
    idle_timeout: Duration,
    finished: bool,
    /// 是否已收到 finish 事件。
    saw_finish: bool,
    usage: Option<crate::sse::Usage>,
}

impl EventStream {
    /// 包一层响应。
    pub fn new(response: reqwest::Response, idle_timeout: Duration) -> Self {
        Self {
            response,
            buffer: LineBuffer::default(),
            pending_lines: std::collections::VecDeque::new(),
            idle_timeout,
            finished: false,
            saw_finish: false,
            usage: None,
        }
    }

    /// 是否已经收到过 finish 事件。
    pub fn saw_finish(&self) -> bool {
        self.saw_finish
    }

    /// 本次生成累计的用量。
    pub fn usage(&self) -> Option<crate::sse::Usage> {
        self.usage
    }

    /// 读取下一个事件。
    ///
    /// 返回 \`Ok(None)\` 表示流正常结束。
    pub async fn next_event(&mut self) -> Result<Option<UpstreamEvent>, CcError> {
        loop {
            if self.finished {
                return Ok(None);
            }
            // 先把缓冲区里已有的完整行消费掉
            if let Some(line) = self.pending_lines.pop_front() {
                if let Some(event) = crate::sse::parse_line(&line) {
                    self.observe(&event);
                    return Ok(Some(event));
                }
                continue;
            }
            // 需要更多数据
            let chunk = tokio::time::timeout(self.idle_timeout, self.response.chunk()).await;
            match chunk {
                Err(_) => {
                    return Err(CcError::StreamIdle(self.idle_timeout.as_millis() as u64));
                }
                Ok(Err(e)) => return Err(CcError::StreamBroken(format!("{e}"))),
                Ok(Ok(None)) => {
                    // 流结束：把最后一行（可能无换行符）也处理掉
                    self.finished = true;
                    if let Some(rest) = self.buffer.take_remainder() {
                        if let Some(event) = crate::sse::parse_line(&rest) {
                            self.observe(&event);
                            return Ok(Some(event));
                        }
                    }
                    return Ok(None);
                }
                Ok(Ok(Some(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    for line in self.buffer.push(&text) {
                        self.pending_lines.push_back(line);
                    }
                }
            }
        }
    }

    /// 记录事件对状态的影响（finish 与 usage）。
    fn observe(&mut self, event: &UpstreamEvent) {
        match event {
            // finish-step 带的是该步用量，finish 带的是总量——后者覆盖前者
            UpstreamEvent::FinishStep { usage: Some(u), .. } => {
                self.usage = Some(*u);
            }
            UpstreamEvent::Finish { usage, .. } => {
                if let Some(u) = usage {
                    self.usage = Some(*u);
                }
                self.saw_finish = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_in_shape_but_random_in_content() {
        let a = generate_fingerprint();
        let b = generate_fingerprint();
        // 结构一致
        assert_eq!(a["components"]["platform"], "win32");
        assert_eq!(b["components"]["platform"], "win32");
        assert!(a["components"]["thumbmark"].is_null());
        assert!(a["thumbmark"].is_string());
        // 内容随机：两次生成的指纹不应相同
        assert_ne!(
            a["thumbmark"], b["thumbmark"],
            "指纹每次生成都应是新的随机值"
        );
        assert_ne!(
            a["components"]["machineIdHash"],
            b["components"]["machineIdHash"]
        );
    }

    #[test]
    fn fingerprint_does_not_leak_host_identity() {
        let fp = generate_fingerprint();
        let text = fp.to_string();
        // 不得包含宿主真实主机名/用户名等（这里只验证没有明显拼进来的痕迹）
        assert!(!text.contains("Users"), "指纹不应泄露本地路径");
        assert_eq!(fp["components"]["runtime"], "cli");
        assert_eq!(fp["components"]["collectorVersion"], 1);
    }

    #[test]
    fn traceparent_matches_w3c_shape() {
        let tp = generate_traceparent();
        let parts: Vec<&str> = tp.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32, "trace id 应为 16 字节");
        assert_eq!(parts[2].len(), 16, "span id 应为 8 字节");
        assert_eq!(parts[3], "01");
    }

    #[test]
    fn endpoint_selection_matches_protocol() {
        let client = UpstreamClient::new(Config::default()).unwrap();
        assert_eq!(
            client.endpoint(UpstreamProtocol::Cli),
            "https://api.commandcode.ai/alpha/generate"
        );
        assert_eq!(
            client.endpoint(UpstreamProtocol::ProviderApi),
            "https://api.commandcode.ai/provider/v1/chat/completions"
        );
        // Auto 目前等价于 CLI：请求体只有 CLI 形状，发给 Provider API 会被拒
        assert_eq!(
            client.endpoint(UpstreamProtocol::Auto),
            "https://api.commandcode.ai/alpha/generate"
        );
    }

    #[test]
    fn api_base_trailing_slash_is_tolerated() {
        let config = Config {
            api_base: "https://example.test/".into(),
            ..Config::default()
        };
        let client = UpstreamClient::new(config).unwrap();
        assert_eq!(
            client.endpoint(UpstreamProtocol::Cli),
            "https://example.test/alpha/generate"
        );
    }

    #[test]
    fn session_is_stable_per_key_and_distinct_across_keys() {
        let client = UpstreamClient::new(Config::default()).unwrap();
        let a1 = client.session_id("key-a");
        let a2 = client.session_id("key-a");
        let b = client.session_id("key-b");
        assert_eq!(a1, a2, "同一 key 在同一周期内应复用 session");
        assert_ne!(a1, b, "不同 key 应使用不同 session");
    }

    #[test]
    fn generate_headers_carry_the_disguise() {
        let client = UpstreamClient::new(Config::default()).unwrap();
        let headers = client.generate_headers("user_test");
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
        };
        assert_eq!(get("authorization"), "Bearer user_test");
        assert_eq!(get("x-cli-environment"), "production");
        assert_eq!(
            get("x-command-code-version"),
            crate::config::DEFAULT_CLI_VERSION
        );
        assert_eq!(get("x-co-flag"), "false");
        assert_eq!(get("x-taste-learning"), "false");
        assert!(!get("x-session-id").is_empty());
        assert!(!get("traceparent").is_empty());
        assert_eq!(get("x-cmd-zdr"), "", "ZDR 默认关闭，不应带该头");
    }

    #[test]
    fn zdr_header_only_when_enabled() {
        let config = Config {
            zdr: true,
            ..Config::default()
        };
        let client = UpstreamClient::new(config).unwrap();
        let headers = client.generate_headers("k");
        assert_eq!(
            headers.get("x-cmd-zdr").and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    #[test]
    fn protocol_preference_defaults_to_cli() {
        // 默认必须是 CLI。我们只构造 CLI 形状的请求体，把那个形状发给
        // Provider API 会被上游拒绝：Invalid input: expected string,
        // received undefined (param: model)——这是真机跑出来的。
        // 这个断言是回归保护：改回 ProviderApi 前必须先补上 Provider 形状的构造器。
        let client = UpstreamClient::new(Config::default()).unwrap();
        assert_eq!(client.initial_protocol("k"), UpstreamProtocol::Cli);
        assert_eq!(client.initial_protocol("another"), UpstreamProtocol::Cli);
    }

    #[test]
    fn learned_cli_only_preference_is_remembered_per_key() {
        let client = UpstreamClient::new(Config::default()).unwrap();
        client.remember_cli_only("k");
        assert_eq!(client.initial_protocol("k"), UpstreamProtocol::Cli);
        assert_eq!(client.initial_protocol("other"), UpstreamProtocol::Cli);
    }

    #[test]
    fn forced_protocol_overrides_cache() {
        let config = Config {
            upstream_protocol: UpstreamProtocol::Cli,
            ..Config::default()
        };
        let client = UpstreamClient::new(config).unwrap();
        client.remember_cli_only("k");
        assert_eq!(client.initial_protocol("k"), UpstreamProtocol::Cli);

        let config = Config {
            upstream_protocol: UpstreamProtocol::ProviderApi,
            ..Config::default()
        };
        let client = UpstreamClient::new(config).unwrap();
        assert_eq!(client.initial_protocol("k"), UpstreamProtocol::ProviderApi);
    }
}
