//! 测试用的 mock 上游服务：真实 HTTP 服务器，用于端到端验证代理逻辑。
//!
//! 移植自 MIT 许可的参考实现，见 THIRD_PARTY.md：
//! - commandcode-proxy 的 proxy.mjs（本仓库固化副本 third_party/proxy.mjs）提供
//!   「上游长什么样」的一手知识：NDJSON 事件、错误码语义、模型目录形状；
//! - 兄弟项目 commandcode-usage 的 mock_server.py 提供「怎么把上游装出来」的形态：
//!   按路径切换形态、请求头可被测试断言、无 Authorization 直接 401。
//!
//! # 为什么放在库 crate，而不是只放 tests/ 下的公共模块
//!
//! tests/ 下的公共模块（mod common;）只能被**同一个** integration 测试二进制用到，
//! 跨 integration 测试文件无法共享；而本 mock 的用途恰恰是给「代理链路」「错误矩阵」
//! 「conformance 对照」等多份集成测试共用（见 docs/PLAN.md 第 8.2、8.3 节）。
//! 因此实现放在库的 src/ 下，并用 #[cfg(any(test, feature = "mock"))] 门控：
//!
//! - **单元测试**（crate 内部 #[cfg(test)]）直接可见，无需任何额外配置；
//! - **本 crate 的 integration 测试**经由 Cargo.toml 里的自引用 dev-dependency
//!   （cc-server = { path = ".", features = ["mock"] }）打开 feature。dev-dependency
//!   只在测试目标里生效，**不会**污染 cargo build 与发布产物的依赖图；
//! - 下游 crate 若也想复用，显式启用 mock feature 即可。
//!
//! # 硬约束（docs/PROTOCOL.md 第 5 节）
//!
//! /alpha/generate 返回的是 **NDJSON**（每行一个 JSON 对象），不是标准 SSE——
//! 既没有 data: 前缀也没有空行分隔事件。参考实现里 parseLine 虽然兼容 data: 前缀
//! （proxy.mjs 约 634 行），但**真实上游不发**；mock 必须复刻真实形状，
//! 否则代理侧的行切分 bug 永远测不出来。
//!
//! # 行为脚本
//!
//! [MockUpstream::start] 接收一串 [MockResponse]（或更细的 [MockBehavior]），
//! 按**收到的请求序号**循环取用：第 N 次（0 起算）请求使用
//! behaviors[N % behaviors.len()]。这让单测可以一行写出「前两次 401/429、
//! 第三次成功」这类轮换场景（见 docs/PLAN.md 第 7 节验收）。
//! [MockBehavior::repeat] 在此基础上支持「前 N 次都给同一个响应」。
//!
//! # 时钟
//!
//! 本模块不在内部读系统时间：需要时间的地方由调用方经 [MockUpstream::start_at]
//! 注入 now_ms，与 pool.rs 的约定一致（docs/STYLE.md 2.3）。这样「请求到达时刻」
//! 可以用确定性的假时钟断言。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
// 只为 BrokenMidStream 里 head.chain(tail) 的 StreamExt::chain 提供方法解析。
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;

/// 生成端点：逆向所得，docs/PROTOCOL.md 第 1 节。
pub const GENERATE_PATH: &str = "/alpha/generate";

/// 模型目录端点：无 key 亦可浏览，docs/PROTOCOL.md 第 1 节。
pub const MODELS_PATH: &str = "/provider/v1/models";

/// Provider API 的对话端点（文档化的 OpenAI 兼容面，需 Pro+ 套餐）。
///
/// mock 必须同时实现它与 /alpha/generate：`upgrade_required` 降级路径的测试
/// 要能看到「先打 Provider API、被拒后再打 CLI」这两次请求落在不同路径上
/// （docs/PROTOCOL.md 第 1、6 节）。
pub const PROVIDER_CHAT_PATH: &str = "/provider/v1/chat/completions";

/// 配额端点（docs/PROTOCOL.md 第 1、7 节）：配额轮询器要打的四个只读端点。
pub const WHOAMI_PATH: &str = "/alpha/whoami";
/// /alpha/billing/credits。
pub const CREDITS_PATH: &str = "/alpha/billing/credits";
/// /alpha/billing/subscriptions（可选带 ?orgId=）。
pub const SUBSCRIPTIONS_PATH: &str = "/alpha/billing/subscriptions";
/// /alpha/usage/summary。
pub const USAGE_SUMMARY_PATH: &str = "/alpha/usage/summary";

/// 四个配额端点的路径清单：mock 用它校验脚本挂载的路径，测试用它遍历。
pub const QUOTA_PATHS: [&str; 4] = [
    WHOAMI_PATH,
    CREDITS_PATH,
    SUBSCRIPTIONS_PATH,
    USAGE_SUMMARY_PATH,
];

/// 默认的 /alpha/whoami 成功体（形状见 docs/PROTOCOL.md 第 7 节）。
///
/// 带上 org.id 是刻意的：订阅端点要用它拼 ?orgId=，少了它那条路径就测不到。
pub const DEFAULT_WHOAMI_BODY: &str =
    r#"{"user":{"id":"user-1","name":"Tester","userName":"tester"},"org":{"id":"org-1"}}"#;
/// 默认的 /alpha/billing/credits 成功体：余额 20、两个窗口、无告警。
pub const DEFAULT_CREDITS_BODY: &str = r#"{"credits":{"monthlyCredits":20,"purchasedCredits":0,"freeCredits":0,"planId":"individual-go"},"windowLimits":{"fiveHour":{"used":1,"cap":10},"weekly":{"used":2,"cap":100}},"belowThreshold":false}"#;
/// 默认的 /alpha/billing/subscriptions 成功体：已知套餐 + 账期结束（月度额度可派生）。
pub const DEFAULT_SUBSCRIPTIONS_BODY: &str = r#"{"data":{"planId":"individual-go","status":"active","currentPeriodEnd":"2027-01-01T00:00:00Z"}}"#;
/// 默认的 /alpha/usage/summary 成功体。
pub const DEFAULT_USAGE_SUMMARY_BODY: &str =
    r#"{"data":{"totalCount":3,"successRate":1,"totalTokensIn":10,"totalTokensOut":5}}"#;

/// /alpha/generate 请求头清单（docs/PROTOCOL.md 第 2 节）。
///
/// 放在库里而不是测试文件里，是为了让后续的 conformance 测试（docs/PLAN.md
/// 第 8.2 节）直接复用同一份清单做逐字段 diff，避免两处各写一份而漂移。
pub const UPSTREAM_REQUEST_HEADERS: [&str; 10] = [
    "content-type",
    "authorization",
    "x-cli-environment",
    "x-command-code-version",
    "x-session-id",
    "x-co-flag",
    "x-taste-learning",
    "x-project-slug",
    "traceparent",
    "x-cmd-zdr",
];

/// 流式成功时默认发出的文本（切成两块以上，覆盖「多事件拼接」路径）。
pub const DEFAULT_STREAM_TEXT: &str = "hello world";

/// 事件之间的间隔（毫秒）。
///
/// 取一个很小但**非零**的值，理由有二：
/// 1. 完全不 sleep 时所有事件会挤进同一个 TCP 段，测不出「按块解帧」的边界；
/// 2. [MockResponse::BrokenMidStream] 必须真的能在流中间断开——否则客户端可能
///    先把整个 body 收完，再看连接错误，那就退化成「连接被拒」而不是「流中断」。
pub const DEFAULT_EVENT_GAP_MS: u64 = 5;

/// 等待 mock 服务器 shutdown 的上限（毫秒）。
///
/// 抽成常量是为了让「这里最多等多久」可见，而不是散落一个裸数字。
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 2_000;

/// 一次生成的默认用量（字段名与上游一致，见 [MockUsage]）。
pub const DEFAULT_USAGE: MockUsage = MockUsage {
    input_tokens: 7,
    output_tokens: 5,
    cached_input_tokens: 0,
};

/// 上游 usage 的形状（camelCase，与 third_party/proxy.mjs 约 690 行一致）。
///
/// 键名必须与 sse::Usage::from_json 读取的 inputTokens / outputTokens /
/// cachedInputTokens 一致，否则 mock 与解析器会各说各话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockUsage {
    /// 输入 token。
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    /// 输出 token。
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    /// 命中缓存的输入 token。
    #[serde(rename = "cachedInputTokens")]
    pub cached_input_tokens: u64,
}

/// 一次请求要给出什么响应。
///
/// 变体与 docs/PLAN.md 第 8.1 节的错误矩阵一一对应：StreamSuccess 覆盖 200 正常流，
/// 其余覆盖拒绝与异常路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockResponse {
    /// 正常流式生成：先发若干 text-delta，再发 finish（NDJSON，不是 SSE）。
    StreamSuccess {
        /// 要流式吐出的完整文本；会被切成若干 text-delta 分块下发。
        text: String,
    },
    /// 返回一个 HTTP 错误（body 由调用方给定，通常放上游的错误 JSON 信封）。
    HttpError {
        /// HTTP 状态码，必须落在 100..=599；非法值在发送时降级为 500。
        status: u16,
        /// 响应体原文。
        body: String,
    },
    /// 返回 403 upgrade_required：该 key 没有 Provider API 权限（Go 套餐）。
    ///
    /// 形状参照 docs/PROTOCOL.md 第 6 节：错误码同时出现在 error.code 与
    /// error.message，让「读 code」与「读 body」两条检测路径都能命中
    /// （error.rs 的 is_upgrade_required 两种都认）。
    UpgradeRequired,
    /// 流中途断开：先发出部分事件，然后在 HTTP 200 之后掐掉连接。
    ///
    /// 这是 docs/PLAN.md 第 8.1 节「200 且流中断 → 不回放、不换号」的专用夹具。
    BrokenMidStream {
        /// 断开前成功发出的文本（至少一个 text-delta）。
        text: String,
    },
    /// 一直不发事件也不结束连接（用于测试流空闲超时）。
    ///
    /// 依赖 docs/PROTOCOL.md #4：上游的读空闲超时只计 read() 等待，
    /// 所以「一个字节都不发」正是空闲超时该触发的形态。
    Hang,
}

impl MockResponse {
    /// 该行为对应的 HTTP 状态码；Hang 会先回 200 头再保持静默，故为 None。
    ///
    /// 把「哪些行为走非 2xx 分支」收敛到一处：UpgradeRequired 虽是独立变体，
    /// 但在 HTTP 层面就是 403。
    pub fn http_status(&self) -> Option<u16> {
        match self {
            MockResponse::StreamSuccess { .. } | MockResponse::BrokenMidStream { .. } => Some(200),
            MockResponse::HttpError { status, .. } => Some(*status),
            MockResponse::UpgradeRequired => Some(403),
            MockResponse::Hang => None,
        }
    }
}

/// 一个**配额端点**在本次调用里返回什么。
///
/// 与生成端点的 [MockResponse] 分开定义：配额端点没有流式形态，只有
/// 「200 + JSON」与「非 2xx + 错误信封」两种，混进同一个枚举会让生成侧的
/// 穷尽匹配凭空多出几个永远走不到的分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockQuotaResponse {
    /// 返回该端点的默认成功体（见各 `DEFAULT_*_BODY` 常量）。
    Default,
    /// 返回指定的状态码与 body 原文。
    Json {
        /// HTTP 状态码，必须落在 100..=599；非法值在发送时降级为 500。
        status: u16,
        /// 响应体原文（通常是上游的错误信封）。
        body: String,
    },
    /// 不返回响应头，一直等到 mock 收摊为止。
    ///
    /// 配额轮询器的整体超时与「上游挂死不会让后台循环停摆」这两条要靠它来测：
    /// 与生成端点的 [MockResponse::Hang] 同形（docs/PROTOCOL.md #4 说明空闲
    /// 只计 read() 等待，所以「一个字节都不发」才是真实的长静默形态）。
    Hang,
}

/// 行为脚本的一个条目：在一段连续的调用区间内决定「这次请求给出什么响应」。
///
/// 整个脚本被拼成一条**时间线**：每个 [MockBehavior] 占据 responses.len() 个
/// 连续槽位（单个 [MockResponse] 占 1 个；[MockBehavior::repeat] 占 times 个；
/// [MockBehavior::sequence] 占响应个数个），第 N 次请求取时间线上第
/// N % 总槽数 个槽位。于是：
///
/// - 传 vec![a, b]（各由 [MockBehavior::from] 转换而来）等价于需求里的
///   「第 N 次用 behaviors[N % len]」，语义完全一致；
/// - [MockBehavior::repeat] 让「连续三次 401，第四次才成功」这类轮换/重试
///   场景一行写完，不必手抄同一个响应；
/// - [MockBehavior::sequence] 在一个区间内给出不同响应，模拟上游抖动。
///
/// [MockBehavior::from] 提供了从单个 [MockResponse] 的便捷转换，因此
/// vec![MockResponse::Hang] 仍然可以直接交给 [MockUpstream::start]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockBehavior {
    /// 该区间内依次使用的响应（个数即区间长度）。
    pub responses: Vec<MockResponse>,
}

impl MockBehavior {
    /// 让接下来的 times 次请求都拿到同一个响应。
    ///
    /// times 为 0 时该条目不占槽位，会被 [select_behavior] 自然跳过，
    /// 不会出现「空区间取模」这类只在测试配置写错时才冒出来的崩溃。
    pub fn repeat(response: MockResponse, times: usize) -> Self {
        Self {
            responses: vec![response; times],
        }
    }

    /// 在一个区间内依次给出这些响应（数量即区间长度）。
    pub fn sequence(responses: Vec<MockResponse>) -> Self {
        Self { responses }
    }

    /// 该条目占用的槽位数。
    fn len(&self) -> usize {
        self.responses.len()
    }
}

impl From<MockResponse> for MockBehavior {
    fn from(response: MockResponse) -> Self {
        Self {
            responses: vec![response],
        }
    }
}

/// 按**调用序号**在行为时间线上选出本次要用的响应。
///
/// 时间线语义见 [MockBehavior]：先按 sequence % 总槽数 定位全局槽位，
/// 再在每个区间内做一次区间内偏移。抽成自由函数是为了能在不启动服务器的情况下
/// 单测取模语义（见文件末尾的 tests 模块）——纯逻辑的验证不该依赖网络。
///
/// 返回 None 表示脚本为空（或全是不占槽位的空区间）：调用方应回 500，而不是猜
/// 一个默认行为——脚本为空几乎必然是测试配置错误，静默回 200 会把错误藏进断言里。
pub fn select_behavior(behaviors: &[MockBehavior], sequence: usize) -> Option<&MockResponse> {
    let total_slots: usize = behaviors.iter().map(MockBehavior::len).sum();
    if total_slots == 0 {
        return None;
    }
    let mut slot = sequence % total_slots;
    for behavior in behaviors {
        if slot < behavior.len() {
            return behavior.responses.get(slot);
        }
        slot -= behavior.len();
    }
    // 上面的循环在 slot < total_slots 时必定命中某个区间；保留这个分支是为了
    // 让未来改动破坏该不变量时返回 None（调用方回 500）而不是 panic。
    None
}

/// 请求头的一条记录：(小写键, 值)。
///
/// 用元组而不是自定义结构体，是为了让断言能直接与
/// UPSTREAM_REQUEST_HEADERS 的清单做逐项比对。
pub type HeaderRecord = (String, String);

/// 一次上游调用的记录；[MockUpstream::requests] 返回它的只读快照。
#[derive(Debug, Clone)]
pub struct ReceivedRequest {
    /// 请求方法。
    pub method: Method,
    /// 请求路径（不含 query）。
    pub path: String,
    /// 完整请求 target：有 query 时形如 `/alpha/billing/subscriptions?orgId=…`。
    ///
    /// 单独记一份是因为订阅端点的 `?orgId=` 正是要断言的行为，只记 [Self::path]
    /// 会把 query 丢掉、让「参数漏拼」这类 bug 测不出来。
    pub target: String,
    /// 请求头，键统一为**小写**，断言时不必关心大小写。
    pub headers: Vec<HeaderRecord>,
    /// 请求体原文（UTF-8 有损解码；mock 只服务 JSON，乱码即说明测试写错了）。
    pub body: String,
    /// 该请求在 mock 内的序号（从 0 起），也是行为脚本取模的依据。
    pub sequence: usize,
    /// 记录时刻（epoch 毫秒），由调用方注入，mock 内部不读系统时间。
    pub received_at_ms: i64,
}

impl ReceivedRequest {
    /// 取一个请求头的值（键不区分大小写；重复出现时取第一个）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(existing, _)| *existing == wanted)
            .map(|(_, value)| value.as_str())
    }

    /// 把请求体解析为 JSON；失败返回 None（由测试自行断言并给出中文说明）。
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// 取出 Authorization 里的 Bearer token（没有则返回 None）。
    ///
    /// 轮换测试要断言「换号前后用了不同的 key」，这里统一剥离 Bearer 前缀，
    /// 免得每个测试各写一遍字符串切片。
    pub fn bearer_token(&self) -> Option<&str> {
        self.header("authorization")?.strip_prefix("Bearer ")
    }
}

/// 一次 mock 调用的共享状态。
struct MockState {
    /// 行为脚本；为空时一律回 500（脚本为空几乎必然是测试配置错误，
    /// 静默回 200 会把错误藏到断言里，难以定位）。取用见 [select_behavior]。
    behaviors: Vec<MockBehavior>,
    /// 已收到的请求（含未命中路径的请求）。
    requests: Mutex<Vec<ReceivedRequest>>,
    /// 注入的当前时刻（epoch 毫秒）。
    now_ms: i64,
    /// Hang 的放行开关：shutdown 置位后，所有悬挂的流一起收尾。
    hang_released: AtomicBool,
    /// **全部生成端点**（/alpha/generate 与 /provider/v1/chat/completions）的调用计数。
    ///
    /// 行为脚本必须由生成请求驱动，不能被指纹录制的预请求（/alpha/fingerprint/record、
    /// /alpha/lifecycle-events）消耗——否则「第 1 次 401、第 2 次成功」这类脚本
    /// 会被预请求吃掉，测试断言的是完全无关的请求序号。
    generate_count: std::sync::atomic::AtomicUsize,
    /// 配额端点的行为脚本：路径 → 响应序列（见 [MockUpstream::set_quota_script]）。
    ///
    /// 键用 `&'static str`：四个端点路径都是编译期常量，这样校验挂载路径时
    /// 不需要分配字符串。
    quota_scripts: Mutex<HashMap<&'static str, Vec<MockQuotaResponse>>>,
    /// 每个配额端点各自已消费的脚本下标（独立循环，互不干扰）。
    quota_cursors: Mutex<HashMap<&'static str, usize>>,
}

impl MockState {
    /// 记下一条请求，补上它的序号并返回。
    fn record(&self, mut request: ReceivedRequest) -> usize {
        // 锁中毒说明另一个测试线程在持锁时 panic 了。这里不能跟着 panic——
        // 那会掩盖最初的失败原因；取回内部数据继续跑，让测试以真实差异报错。
        let mut guard = self
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        request.sequence = guard.len();
        let sequence = request.sequence;
        guard.push(request);
        sequence
    }

    /// 已收到的请求快照（按到达顺序）。
    ///
    /// 返回克隆而非锁守卫：守卫会把锁类型泄漏进公开 API，并让断言失败时的
    /// 展开路径更啰嗦。测试的请求量很小，克隆成本可忽略。
    fn snapshot(&self) -> Vec<ReceivedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// 测试用的 mock 上游服务。
///
/// 持有真实监听的 axum 服务器、行为脚本与请求记录。[Drop] 会发出关闭信号，
/// 因此测试里通常不必手工收尾；需要确认「端口已释放」时再显式调用
/// [MockUpstream::shutdown]。
pub struct MockUpstream {
    /// 实际监听的地址（端口随机，start 之后才知道）。
    addr: SocketAddr,
    /// mock 的基址，方便直接拼 URL：http://127.0.0.1:{port}。
    base_url: String,
    /// 共享状态。
    state: Arc<MockState>,
    /// 关闭信号。
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// 服务器任务句柄；仅在显式 shutdown 时 join，避免阻塞测试。
    server_task: Option<tokio::task::JoinHandle<()>>,
    /// 已请求关闭的标记，让 shutdown 幂等。
    shutdown_requested: bool,
}

impl MockUpstream {
    /// 启动 mock 服务器，监听 127.0.0.1:0（端口由内核分配），返回实际端口。
    ///
    /// 必须在 Tokio 运行时内调用（内部使用 tokio::net::TcpListener）。
    /// 时间基准取 0：需要确定性时间戳的测试请用 [MockUpstream::start_at]。
    ///
    /// 接受裸 [MockResponse] 列表（每个默认成一个单元素窗口）或显式的
    /// [MockBehavior] 列表，见 [MockBehavior] 的说明。
    pub async fn start(behaviors: impl IntoIterator<Item = impl Into<MockBehavior>>) -> Self {
        Self::start_at(behaviors, 0).await
    }

    /// 同 [MockUpstream::start]，但把 mock 看到的「当前时间」固定为 now_ms。
    ///
    /// 时间从参数注入（docs/STYLE.md 2.3）：断言
    /// [ReceivedRequest::received_at_ms] 的测试可以给出确定值。
    pub async fn start_at(
        behaviors: impl IntoIterator<Item = impl Into<MockBehavior>>,
        now_ms: i64,
    ) -> Self {
        let state = Arc::new(MockState {
            behaviors: behaviors.into_iter().map(Into::into).collect(),
            requests: Mutex::new(Vec::new()),
            now_ms,
            hang_released: AtomicBool::new(false),
            generate_count: std::sync::atomic::AtomicUsize::new(0),
            quota_scripts: Mutex::new(HashMap::new()),
            quota_cursors: Mutex::new(HashMap::new()),
        });

        let app = Router::new()
            // docs/PROTOCOL.md 第 1 节：生成端点与模型目录端点。
            // 两者都用 any(..) 注册，理由见各自 handler 的注释。
            .route(GENERATE_PATH, any(handle_generate))
            // Provider API 面与 CLI 面共用同一个 handler：上游两种承载的事件流
            // 语义一致（mock 侧统一发 NDJSON），测试关心的是「打到了哪条路径」。
            .route(PROVIDER_CHAT_PATH, any(handle_generate))
            .route(MODELS_PATH, any(handle_models))
            // 配额端点（docs/PROTOCOL.md 第 7 节）：默认成功体 + 可注入脚本，
            // 让配额轮询器的「独立降级 / 401 短路」能被端到端验证。
            .route(WHOAMI_PATH, any(handle_quota))
            .route(CREDITS_PATH, any(handle_quota))
            .route(SUBSCRIPTIONS_PATH, any(handle_quota))
            .route(USAGE_SUMMARY_PATH, any(handle_quota))
            .fallback(any(handle_not_found))
            .with_state(Arc::clone(&state));

        // 端口 0 让内核挑空闲端口，避免测试之间抢端口。
        //
        // 这里的 expect 属于 docs/STYLE.md 2.4 允许的「测试代码」范畴（本模块由
        // feature 门控，只为测试服务）；绑定 loopback 的 0 号端口在正常环境里
        // 不可能失败，失败即代表运行环境异常，用一句中文说明把它顶出来更省排查时间。
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock 上游应能绑定 127.0.0.1 的随机空闲端口");
        let addr = listener
            .local_addr()
            .expect("TcpListener 在成功 bind 之后 local_addr 必定成功");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    // 发送端被显式 send 或随 Drop 释放都会让这里醒来，两种情况都该收摊。
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        Self {
            addr,
            base_url: format!("http://{addr}"),
            state,
            shutdown_tx: Some(shutdown_tx),
            server_task: Some(server_task),
            shutdown_requested: false,
        }
    }

    /// 实际监听端口。
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// mock 的基址（http://127.0.0.1:{port}），供被测代理拼上游地址。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 已收到的全部请求（按到达顺序）。
    pub fn requests(&self) -> Vec<ReceivedRequest> {
        self.state.snapshot()
    }

    /// 已收到的请求数。
    pub fn request_count(&self) -> usize {
        self.state.snapshot().len()
    }

    /// 生成端点的调用次数（CLI 面与 Provider 面合计，预请求不计）。
    ///
    /// 断言「轮换了几次」时必须用它，而不是 [Self::request_count]——后者把
    /// 指纹录制、生命周期等预请求也算进去，得出的数字与轮换无关
    /// （见 MockState::generate_count 的文档）。
    pub fn generate_count(&self) -> usize {
        self.state
            .generate_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 给某个配额端点挂一个响应序列（按该端点自己的调用次数循环取用）。
    ///
    /// 配额端点的行为没法塞进 [MockBehavior] 那条时间线：四个端点各自独立，
    /// 而时间线是**全局**按请求序号取模的——whoami 的一次失败会把 credits 的
    /// 脚本也往前推一格。这里按路径分开计数，正好对应
    /// 「某个端点失败不影响其余端点」这条要测的语义（docs/PROTOCOL.md 第 7 节）。
    ///
    /// 路径不在 [QUOTA_PATHS] 里时返回 false 且**不挂载**：静默挂到一个永不被路由的
    /// 路径上会让测试以为脚本生效了，反而更难排查。
    pub fn set_quota_script(&self, path: &'static str, responses: Vec<MockQuotaResponse>) -> bool {
        if !QUOTA_PATHS.contains(&path) {
            return false;
        }
        self.state
            .quota_scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path, responses);
        self.state
            .quota_cursors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path, 0);
        true
    }

    /// 只包含**生成请求**的记录（排除指纹录制、生命周期等预请求）。
    ///
    /// 断言「用了哪个 key」「打到了哪条路径」时必须用它：预请求带着同一个
    /// Bearer 头，混在一起会让 token 序列看起来像重复换号。
    pub fn generate_requests(&self) -> Vec<ReceivedRequest> {
        self.state
            .snapshot()
            .into_iter()
            .filter(|r| r.path == GENERATE_PATH || r.path == PROVIDER_CHAT_PATH)
            .collect()
    }

    /// 显式关闭：放行所有 Hang 的连接、停止接受新连接并等待收尾。
    ///
    /// [Drop] 做同样的事，但 Drop 里不能 await；需要「关闭已完成」这一保证时
    /// （例如断言端口不再监听、或避免服务器任务跨测试存活）再显式调用。
    /// 等待有上限（[DEFAULT_SHUTDOWN_TIMEOUT_MS]），不会把测试挂死。
    pub async fn shutdown(&mut self) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        // 顺序重要：先放行 Hang 的流，再发关闭信号，最后 join，
        // 否则优雅关闭会一直等那条永远不会自己结束的流。
        self.state.hang_released.store(true, Ordering::SeqCst);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.server_task.take() {
            let _ = tokio::time::timeout(Duration::from_millis(DEFAULT_SHUTDOWN_TIMEOUT_MS), task)
                .await;
        }
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        // Drop 里不能 await，只做两件同步的事：
        // 1) 放行 Hang 的流（否则它永远停在 Pending，服务器任务无法优雅收尾）；
        // 2) 发关闭信号并 abort 服务器任务，保证不留后台连接。
        //
        // 注意 drop 发生在测试结束、runtime 可能已经关闭时：abort 只是打标记，
        // 此时不会有任何副作用。
        self.state.hang_released.store(true, Ordering::SeqCst);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

/// 生成端点处理器：**所有方法都落到这里**。
///
/// 用 any(..) 而不是 post(..) 是刻意的：方法不对时测试能拿到一条真实记录，
/// 而不是 axum 默认的 405 + 空 body——后者会让「mock 没收到请求」与
/// 「收到了但方法错了」难以区分。
async fn handle_generate(
    State(state): State<Arc<MockState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    state.record(ReceivedRequest {
        method,
        path: uri.path().to_string(),
        target: request_target(&uri),
        headers: collect_headers(&headers),
        body: String::from_utf8_lossy(&body).into_owned(),
        sequence: 0,
        received_at_ms: state.now_ms,
    });

    // 行为脚本只看生成请求的序号：预请求不该消耗脚本（见 generate_count 的文档）
    let sequence = state
        .generate_count
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let behavior = match select_behavior(&state.behaviors, sequence) {
        Some(behavior) => behavior.clone(),
        None => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "mock_upstream: 行为脚本为空，无法决定如何响应",
            );
        }
    };

    match behavior {
        MockResponse::StreamSuccess { text } => stream_success_response(&text),
        MockResponse::BrokenMidStream { text } => broken_stream_response(&text),
        MockResponse::HttpError { status, body } => {
            // 非法状态码降级为 500 而不是 panic：mock 是测试基础设施，
            // 它自己崩掉会把「测试写错」伪装成「被测代码挂掉」。
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_body(code, &body)
        }
        MockResponse::UpgradeRequired => json_body(
            StatusCode::FORBIDDEN,
            // docs/PROTOCOL.md 第 6 节：403 upgrade_required 要「固定降级到
            // /alpha/generate」。code 与 message 都带上 upgrade_required，
            // 让 error.rs 里读 code / 读 body 的两条路径都能命中。
            r#"{"error":{"code":"upgrade_required","message":"upgrade_required: Provider API is not available on this plan"}}"#,
        ),
        MockResponse::Hang => hang_response(Arc::clone(&state)),
    }
}

/// 模型目录端点：返回 OpenAI 形状的 {"object":"list","data":[...]}。
///
/// 形状对照 third_party/proxy.mjs 约 2150 行：那里只读 data[].id；
/// mock 额外补上 object / owned_by，方便被测的 /v1/models 直接透传。
/// 同样用 any(..) 注册，以便方法错误时留下记录。
async fn handle_models(
    State(state): State<Arc<MockState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    state.record(ReceivedRequest {
        method,
        path: uri.path().to_string(),
        target: request_target(&uri),
        headers: collect_headers(&headers),
        body: String::new(),
        sequence: 0,
        received_at_ms: state.now_ms,
    });
    json_body(
        StatusCode::OK,
        r#"{"object":"list","data":[{"id":"deepseek/deepseek-v4-pro","object":"model","owned_by":"commandcode"},{"id":"deepseek/deepseek-v4-flash","object":"model","owned_by":"commandcode"}]}"#,
    )
}

/// 配额端点处理器：四个只读端点共用。
///
/// 默认给成功体；测试可用 [MockUpstream::set_quota_script] 覆盖某个端点。
/// 同样用 any(..) 注册，方法错误时也能留下记录。
async fn handle_quota(
    State(state): State<Arc<MockState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let target = request_target(&uri);
    state.record(ReceivedRequest {
        path: uri.path().to_string(),
        target: target.clone(),
        method,
        headers: collect_headers(&headers),
        body: String::new(),
        sequence: 0,
        received_at_ms: state.now_ms,
    });

    let path = uri.path().to_string();
    // 路径一定来自路由表（必然是 QUOTA_PATHS 之一），取默认体是安全的兜底。
    let default_body = default_quota_body(&path);
    let response = {
        let mut scripts = state
            .quota_scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match scripts.get_mut(path.as_str()) {
            Some(responses) if !responses.is_empty() => {
                let mut cursors = state
                    .quota_cursors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let cursor = cursors.entry(quota_path_of(&path)).or_insert(0);
                let picked = responses[*cursor % responses.len()].clone();
                *cursor += 1;
                picked
            }
            // 没挂脚本（或挂了空序列）时回默认成功体：空序列几乎必然是测试写错，
            // 但这里不该让整个 mock 变成 500，否则会把「配额逻辑错」伪装成
            // 「mock 自己坏了」。
            _ => MockQuotaResponse::Default,
        }
    };

    match response {
        MockQuotaResponse::Default => json_body(StatusCode::OK, default_body),
        MockQuotaResponse::Json { status, body } => {
            // 非法状态码降级为 500 而不是 panic（与生成端点的处理一致）。
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_body(code, &body)
        }
        MockQuotaResponse::Hang => quota_hang_response(Arc::clone(&state)),
    }
}

/// 把命中路径映射回 [QUOTA_PATHS] 里的常量，供游标表当键用。
///
/// 走到这里的路径必然来自路由表，因此兜底返回 whoami 只是一个「让类型收敛」
/// 的选择；即便如此也不会 panic。
fn quota_path_of(path: &str) -> &'static str {
    QUOTA_PATHS
        .into_iter()
        .find(|known| *known == path)
        .unwrap_or(WHOAMI_PATH)
}

/// 某个配额端点的默认成功体。
fn default_quota_body(path: &str) -> &'static str {
    match quota_path_of(path) {
        CREDITS_PATH => DEFAULT_CREDITS_BODY,
        SUBSCRIPTIONS_PATH => DEFAULT_SUBSCRIPTIONS_BODY,
        USAGE_SUMMARY_PATH => DEFAULT_USAGE_SUMMARY_BODY,
        _ => DEFAULT_WHOAMI_BODY,
    }
}

/// 一直不返回响应头的配额端点响应（超时夹具）。
///
/// 与 [hang_response] 的区别是它不设 Content-Type、不留任何 body 语义：
/// 它要模拟的是「连接建立了但上游一句话都不说」，用于验证调用方的整体超时。
fn quota_hang_response(state: Arc<MockState>) -> Response {
    let released = Arc::clone(&state);
    let body_stream = futures_util::stream::once(async move {
        while !released.hang_released.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(DEFAULT_EVENT_GAP_MS)).await;
        }
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    });
    Response::new(Body::from_stream(body_stream))
}

/// 取出请求的完整 target（路径 + query），并归一化成以 `/` 开头的形式。
///
/// 直接 `uri.to_string()` 在 HTTP/2 下会带上 authority 形式（`scheme://host/path`），
/// 断言 query 时会平白多出噪声；`path_and_query` 取到的才是转发语义上的 target。
fn request_target(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|value| value.to_string())
        .unwrap_or_else(|| uri.path().to_string())
}

/// 未命中的路径：回 404，但**仍然记录**这次请求。
///
/// 记录是关键——被测代理打错路径时，测试要能从 requests() 看出「它到底打到了哪」，
/// 而不是只看到一个 404。
async fn handle_not_found(
    State(state): State<Arc<MockState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    state.record(ReceivedRequest {
        method,
        path: uri.path().to_string(),
        target: request_target(&uri),
        headers: collect_headers(&headers),
        body: String::from_utf8_lossy(&body).into_owned(),
        sequence: 0,
        received_at_ms: state.now_ms,
    });
    json_error(
        StatusCode::NOT_FOUND,
        "mock_upstream: 未实现的端点（本 mock 只服务 /alpha/generate、/provider/v1/models 与 /alpha/* 配额端点）",
    )
}

/// 正常流式响应：text-delta 若干 + finish，逐行 NDJSON。
fn stream_success_response(text: &str) -> Response {
    let mut events = text_delta_events(text);
    events.push(serde_json::json!({
        "type": "finish",
        "finishReason": "stop",
        "totalUsage": DEFAULT_USAGE,
    }));
    ndjson_response(&events)
}

/// 流中途断开的响应。
///
/// 实现要点：body 用一个「先吐一块、再吐一个 io::Error」的流。hyper 在 body
/// 出错时只能**掐断连接**（它无法再补一个合法的结束帧），于是客户端会先拿到
/// 部分 text-delta，随后在读取时拿到传输错误——这正是 docs/PLAN.md 第 8.1 节
/// 「200 且流中断」要覆盖的路径。
///
/// 之所以不用「谎报 Content-Length」的写法：那依赖 hyper 对长度不符的处理，
/// 而 body 泄露一个错误是标准且与 HTTP/1.1、HTTP/2 都无关的断流方式。
fn broken_stream_response(text: &str) -> Response {
    let events = text_delta_events(text);
    // 断流点取前一半（至少一块）：保证「已经收到 200 与部分内容」这一前提成立。
    let keep = events.len().div_ceil(2).max(1);
    let partial = encode_ndjson(&events[..keep.min(events.len())]);
    let gap = Duration::from_millis(DEFAULT_EVENT_GAP_MS);

    let body_stream = futures_util::stream::once(async move {
        // 先让部分事件真正落地（客户端可能已经读到），再制造断流。
        tokio::time::sleep(gap).await;
        Err::<Vec<u8>, std::io::Error>(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "mock_upstream: 故意在流中途断开（BrokenMidStream）",
        ))
    });
    let head =
        futures_util::stream::once(
            async move { Ok::<Vec<u8>, std::io::Error>(partial.into_bytes()) },
        );
    let mut response = Response::new(Body::from_stream(head.chain(body_stream)));
    *response.status_mut() = StatusCode::OK;
    set_content_type(&mut response, "application/x-ndjson");
    response
}

/// 一直不发事件的响应（流空闲超时夹具）。
///
/// 依赖 docs/PROTOCOL.md #4：只有 read() 等待时间算空闲，所以「一个字节都不发」
/// 才符合真实上游的长思考形态。body 流在收到 shutdown 放行前一直处于 Pending；
/// 客户端提前断开时，axum 会丢弃 body，这个 future 也随之被 drop。
fn hang_response(state: Arc<MockState>) -> Response {
    let released = Arc::clone(&state);
    let body_stream = futures_util::stream::once(async move {
        while !released.hang_released.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(DEFAULT_EVENT_GAP_MS)).await;
        }
        Ok::<Vec<u8>, std::io::Error>(Vec::new())
    });
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = StatusCode::OK;
    set_content_type(&mut response, "application/x-ndjson");
    response
}

/// 把文本切成若干 text-delta 事件。
///
/// 空文本返回空列表：只有 finish 事件也是合法上游行为（例如模型被立即截断），
/// 是必须覆盖的边界。其余按 8 个字符切块，保证多事件路径被覆盖。
fn text_delta_events(text: &str) -> Vec<Value> {
    const CHUNK_CHARS: usize = 8;
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(CHUNK_CHARS)
        .map(|chunk| {
            // docs/PROTOCOL.md 第 5 节：文本增量事件形如 {"type":"text-delta","text":...}。
            serde_json::json!({ "type": "text-delta", "text": chunk.iter().collect::<String>() })
        })
        .collect()
}

/// 把事件列表编码为 NDJSON（每行一个 JSON + 换行）。
///
/// 刻意**不**加 data: 前缀、也不加空行：真实上游不是标准 SSE
/// （docs/PROTOCOL.md 第 5 节），mock 必须复刻真实形状。
fn encode_ndjson(events: &[Value]) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str(&event.to_string());
        out.push('\n');
    }
    out
}

/// 构造 NDJSON 响应。
fn ndjson_response(events: &[Value]) -> Response {
    let mut response = Response::new(Body::from(encode_ndjson(events)));
    *response.status_mut() = StatusCode::OK;
    set_content_type(&mut response, "application/x-ndjson");
    response
}

/// 构造一个 JSON 响应（body 由调用方保证是合法 JSON，mock 只服务 JSON）。
fn json_body(status: StatusCode, body: &str) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    set_content_type(&mut response, "application/json");
    response
}

/// 构造统一形状的错误 JSON（mock 自身出错时用，与上游信封区分开）。
fn json_error(status: StatusCode, message: &str) -> Response {
    let body =
        serde_json::json!({ "error": { "code": "mock_upstream_error", "message": message } });
    json_body(status, &body.to_string())
}

/// 设置 Content-Type；构造失败时宁可不设头也不 panic（mock 不该因自身基础设施报错）。
fn set_content_type(response: &mut Response, value: &str) {
    if let Ok(header_value) = axum::http::HeaderValue::from_str(value) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, header_value);
    }
}

/// 收集请求头，键统一小写；非 ASCII 的值退化为空串（测试只断言 ASCII 头）。
fn collect_headers(headers: &HeaderMap) -> Vec<HeaderRecord> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! mock 的**纯逻辑**单测：不启动服务器、不碰网络。
    //!
    //! 网络侧行为由 tests/mock_upstream.rs 的集成测试覆盖；这里只钉住行为脚本的
    //! 取模语义——它错了的话，所有依赖 mock 的测试都会以**错误的方式继续跑**
    //! （例如重试测试其实根本没重试），这比直接失败更危险。

    use super::*;

    /// 便利：构造一个只带文本的成功响应。
    fn success(text: &str) -> MockResponse {
        MockResponse::StreamSuccess {
            text: text.to_string(),
        }
    }

    #[test]
    fn single_window_cycles_the_same_response_for_every_sequence() {
        let behaviors = vec![MockBehavior::from(success("a"))];
        for sequence in 0..5 {
            assert_eq!(
                select_behavior(&behaviors, sequence),
                Some(&success("a")),
                "只有一档行为时，任意序号的请求都应拿到它"
            );
        }
    }

    #[test]
    fn windows_are_selected_by_sequence_and_wrap_around() {
        let behaviors = vec![
            MockBehavior::from(MockResponse::HttpError {
                status: 401,
                body: "{}".into(),
            }),
            MockBehavior::from(success("ok")),
        ];
        assert_eq!(
            select_behavior(&behaviors, 0).and_then(MockResponse::http_status),
            Some(401),
            "序号 0 用第一个窗口：连续 401 之后才成功（PLAN.md 第 7 节验收场景）"
        );
        assert_eq!(
            select_behavior(&behaviors, 1).and_then(MockResponse::http_status),
            Some(200)
        );
        assert_eq!(
            select_behavior(&behaviors, 2).and_then(MockResponse::http_status),
            Some(401),
            "序号 2 应回到第一个窗口（取模循环）"
        );
    }

    #[test]
    fn repeat_window_holds_the_response_for_exactly_times_calls() {
        let flaky = MockResponse::HttpError {
            status: 429,
            body: "{}".into(),
        };
        let behaviors = vec![
            MockBehavior::repeat(flaky.clone(), 3),
            MockBehavior::from(success("recovered")),
        ];
        for sequence in 0..3 {
            assert_eq!(
                select_behavior(&behaviors, sequence),
                Some(&flaky),
                "重复窗口的前 3 次（序号 0..=2）都应是 429"
            );
        }
        assert_eq!(
            select_behavior(&behaviors, 3),
            Some(&success("recovered")),
            "第 4 次（序号 3）切换到下一个窗口"
        );
        assert_eq!(
            select_behavior(&behaviors, 4),
            Some(&flaky),
            "时间线共 4 个槽位，序号 4 取模回到第一个槽位（429）——整条时间线循环"
        );
    }

    #[test]
    fn sequence_window_advances_within_the_window() {
        let behaviors = vec![MockBehavior::sequence(vec![
            MockResponse::HttpError {
                status: 401,
                body: "{}".into(),
            },
            MockResponse::HttpError {
                status: 429,
                body: "{}".into(),
            },
            success("third"),
        ])];
        let statuses: Vec<Option<u16>> = (0..3)
            .map(|sequence| {
                select_behavior(&behaviors, sequence).and_then(MockResponse::http_status)
            })
            .collect();
        assert_eq!(
            statuses,
            vec![Some(401), Some(429), Some(200)],
            "一个窗口内应按序号依次给出不同响应，覆盖「上游抖动」场景"
        );
    }

    #[test]
    fn empty_script_yields_none_instead_of_panicking() {
        // 空脚本是测试配置错误：必须显式返回 None（调用方回 500），不能取模崩溃
        assert!(select_behavior(&[], 0).is_none());
    }

    #[test]
    fn empty_window_is_skipped_without_dividing_by_zero() {
        let behaviors = vec![MockBehavior::repeat(success("unused"), 0)];
        assert!(
            select_behavior(&behaviors, 0).is_none(),
            "times=0 的空窗口应被安全跳过，而不是除零 panic"
        );
    }

    #[test]
    fn http_status_matches_the_error_matrix() {
        // docs/PLAN.md 第 8.1 节错误矩阵在 HTTP 层面的取值
        assert_eq!(success("x").http_status(), Some(200));
        assert_eq!(MockResponse::UpgradeRequired.http_status(), Some(403));
        assert_eq!(
            MockResponse::HttpError {
                status: 402,
                body: String::new()
            }
            .http_status(),
            Some(402),
            "402 必须原样呈现：上游把它折叠成 429 是代理层要处理的坑，mock 不能提前折叠"
        );
        assert_eq!(
            MockResponse::Hang.http_status(),
            None,
            "Hang 先回 200 头再保持静默，没有「最终状态码」可言"
        );
    }

    #[test]
    fn ndjson_encoding_is_one_json_object_per_line() {
        let events = vec![
            serde_json::json!({"type": "text-delta", "text": "a"}),
            serde_json::json!({"type": "finish"}),
        ];
        let encoded = encode_ndjson(&events);
        assert_eq!(
            encoded.lines().count(),
            2,
            "NDJSON 必须一行一个事件（PROTOCOL.md 第 5 节），不能用 SSE 的空行分隔"
        );
        assert!(encoded.ends_with('\n'), "每个事件都应以换行收尾");
        assert!(
            !encoded.contains("data:"),
            "真实上游不发 data: 前缀，mock 必须复刻真实形状"
        );
    }

    #[test]
    fn text_is_split_into_multiple_deltas_and_concatenates_back() {
        let events = text_delta_events(DEFAULT_STREAM_TEXT);
        assert!(
            events.len() >= 2,
            "示例文本应至少切成两块，才能覆盖多事件路径"
        );
        let joined: String = events
            .iter()
            .map(|event| event["text"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(joined, DEFAULT_STREAM_TEXT, "分块拼接必须原样还原文本");
    }

    #[test]
    fn empty_text_produces_no_delta_events() {
        assert!(
            text_delta_events("").is_empty(),
            "空文本是合法边界：只发 finish 也是上游可能的行为"
        );
    }
}
