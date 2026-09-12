//! 配额轮询器：周期性用每个账号的 key 去拉 /alpha/* 的四个配额端点。
//!
//! 本模块只做**网络与调度**这一件事，解析全部交给 [crate::quota]：
//! 端点结果装进 [crate::quota::EndpointResponses] 后调用
//! [crate::quota::build_snapshot]，再把快照推给订阅者。
//! 移植自 third_party/usage-worker.js 的 `fetchReport` / `refreshAccount`（MIT，见
//! THIRD_PARTY.md），并保留它的全部容错形状：
//!
//! - **四个端点独立降级**：某个端点失败时其余端点仍要报告，失败原因汇总进
//!   [crate::quota::QuotaSnapshot::last_error]。见 docs/PROTOCOL.md 第 7 节。
//! - **401/403 短路**：whoami 返回 401/403 就已经说明密钥无效，继续打另外三个
//!   端点既浪费、又会重复触发上游的风控计数（参考实现的 `fetchReport` 同样是
//!   `throw` 掉后续调用）。短路时只发一条订阅更新，让面板立刻显示「密钥被拒」。
//! - **探针失败不得改变账号池状态**：本模块**刻意不持有** [crate::pool::AccountPool]。
//!   配额探测是只读的旁观者，探测失败只影响面板展示，绝不能把账号标成不可用。
//!   见 docs/PROTOCOL.md 第 7 节末段。
//!
//! # 时间与可测性
//!
//! - 轮询**间隔**由调用方经参数注入（生产取 45s，测试取 50ms 这类很短的值），
//!   生产默认值见 [DEFAULT_INTERVAL_MS]，不在本模块内部写死。
//! - 快照的 `now_ms` 与 [QuotaUpdate::at_ms] 经 [QuotaPoller::new_with_clock] 注入时钟取值，
//!   默认实现是 [crate::upstream::now_ms]；这样定时器与快照时间戳都能被确定性测试
//!   覆盖，符合 docs/STYLE.md 第 2.3 节「不在函数内部读系统时间」的约定。
//! - 单次轮询的**整体**耗时上限由 [QuotaPoller::with_timeout] 注入。没有它的话，
//!   一个挂死的上游连接会让整个轮询循环卡住，面板跟着停止刷新。
//!
//! # 串行而非并发
//!
//! 各账号之间**串行**探测（docs/ARCHITECTURE.md 第 1 节「串行探测各账号（避免惊群）」）。
//! 账号数是个位数，串行省下的复杂度（限流、并发上限、乱序更新）远多于它多花的时间。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::config::Config;
use crate::error::CcError;
use crate::pool::AccountSlot;
use crate::quota::{build_snapshot, AccountIdentity, EndpointResponses, QuotaSnapshot};
use crate::upstream::{now_ms, UpstreamClient};

/// 生产环境的轮询间隔（毫秒）：45s。
///
/// 配额由 45s 轮询产生，见 docs/ARCHITECTURE.md 第 1、5 节（上游不是实时数据源，
/// 没必要也不应该加密轮询）。调用方仍可注入别的间隔，测试用 50ms 这类很短的值。
pub const DEFAULT_INTERVAL_MS: u64 = 45_000;

/// 单次轮询（一个账号的四个端点）的整体耗时上限（毫秒）。
///
/// 必须存在：否则一个挂死的连接会让后台循环停摆，面板永远停在旧数据。
/// 取 30s 是因为它明显小于 45s 的轮询周期——单号偏慢也不会让下一轮迟到。
pub const DEFAULT_POLL_TIMEOUT_MS: u64 = 30_000;

/// 订阅通道的容量。
///
/// 配额更新是**最新值优先**的可丢消息：订阅者落后时丢掉旧快照比阻塞轮询器更合理
/// （[broadcast::error::RecvError::Lagged] 即此语义）。
const UPDATE_CHANNEL_CAPACITY: usize = 256;

/// 需要探测的四个配额端点。见 docs/PROTOCOL.md 第 1 节与第 7 节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaEndpoint {
    /// `GET /alpha/whoami`——身份，及其 orgId（订阅端点要用它拼 ?orgId=）。
    Whoami,
    /// `GET /alpha/billing/credits`——余额与 5 小时 / 周窗口。
    Credits,
    /// `GET /alpha/billing/subscriptions`——套餐、状态、账期结束。
    Subscriptions,
    /// `GET /alpha/usage/summary`——计费周期汇总。
    UsageSummary,
}

impl QuotaEndpoint {
    /// 端点路径（不含 apiBase）。订阅端点的 `?orgId=` 由调用方按需拼接。
    fn path(self) -> &'static str {
        match self {
            QuotaEndpoint::Whoami => "/alpha/whoami",
            QuotaEndpoint::Credits => "/alpha/billing/credits",
            QuotaEndpoint::Subscriptions => "/alpha/billing/subscriptions",
            QuotaEndpoint::UsageSummary => "/alpha/usage/summary",
        }
    }

    /// 结构化日志与失败汇总里使用的稳定标识（英文）。
    fn name(self) -> &'static str {
        match self {
            QuotaEndpoint::Whoami => "whoami",
            QuotaEndpoint::Credits => "billing/credits",
            QuotaEndpoint::Subscriptions => "billing/subscriptions",
            QuotaEndpoint::UsageSummary => "usage/summary",
        }
    }
}

/// 单次配额探测的错误。
///
/// 一个端点失败**不会**让整个账号失败——调用方把 `Err` 放进
/// [EndpointResponses] 对应槽位后继续探测其余端点（docs/PROTOCOL.md 第 7 节）。
/// 只有 whoami 的 401/403 是例外：那说明密钥无效，后续端点不再发起。
///
/// 对应参考实现 `getJson` 抛出的那个错误对象（status + 截断后的 body）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuotaFetchError {
    /// 端点返回非 2xx。
    #[error("HTTP {status}{}", code_suffix(.code))]
    Http {
        /// HTTP 状态码。
        status: u16,
        /// 上游 `error.code`（若 body 是可解析的 JSON 信封）。
        code: Option<String>,
        /// 错误响应体原文，由 [UpstreamClient::classify_error] **截断**后给出。
        body: String,
    },

    /// 传输层失败（DNS、连接被拒、TLS、代理、连接重置）。
    #[error("请求失败：{0}")]
    Transport(String),

    /// 超过本轮探测的整体耗时上限。
    #[error("请求在 {0} 毫秒内未完成")]
    Timeout(u64),

    /// 响应体不是合法 JSON。
    #[error("响应不是合法 JSON：{0}")]
    Decode(String),
}

/// 给 `Http` 渲染 ` (code)` 后缀。
fn code_suffix(code: &Option<String>) -> String {
    code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
}

impl QuotaFetchError {
    /// 该错误是否意味着「密钥无效」。
    ///
    /// 只认 401/403：上游把多种业务拒绝折叠进 403，但 `/alpha/*` 是只读端点，
    /// 403 在这里同样表示该 key 没有访问权限，继续探测毫无意义。
    /// 见 docs/PROTOCOL.md 第 6 节与第 7 节。
    pub fn is_credential_rejected(&self) -> bool {
        matches!(self, QuotaFetchError::Http { status, .. } if *status == 401 || *status == 403)
    }

    /// 该错误对应的 HTTP 状态码；非 HTTP 错误返回 0（只用于日志字段）。
    fn status(&self) -> u16 {
        match self {
            QuotaFetchError::Http { status, .. } => *status,
            _ => 0,
        }
    }

    /// 面向用户的中文提示（错误信息三段式：做什么 / 为什么 / 怎么办）。
    ///
    /// 保留 `status` 的原因与参考实现一致：用户要拿这个数字去核对
    /// commandcode.ai 上的密钥状态，笼统的「请求失败」帮不上忙。
    /// 文案里**只**出现状态码，绝不回显 key。
    pub fn user_message(&self) -> String {
        match self {
            QuotaFetchError::Http { status, .. } if self.is_credential_rejected() => format!(
                "Command Code 密钥被拒（HTTP {status}）——请确认密钥有效（commandcode.ai/settings），或在设置页重新登录"
            ),
            QuotaFetchError::Http { status, .. } => {
                format!("配额接口返回 HTTP {status}——稍后重试；若持续失败请检查账号状态")
            }
            QuotaFetchError::Transport(reason) => format!(
                "无法连接 Command Code 配额接口：{reason}——请检查网络或代理设置后重试"
            ),
            QuotaFetchError::Timeout(limit_ms) => format!(
                "配额接口在 {limit_ms} 毫秒内没有响应——已中止本轮探测，下一轮会自动重试"
            ),
            QuotaFetchError::Decode(reason) => format!(
                "配额接口返回的内容不是 JSON：{reason}——通常是上游临时异常，下一轮会自动重试"
            ),
        }
    }
}

/// 把核心层的错误映射成配额轮询的错误类型。
///
/// 保留 [CcError] 的语义而不是拍平成字符串：401/403 的判定（短路）与
/// [QuotaPoller::poll_once] 的独立降级都依赖它。
fn map_cc_error(error: CcError) -> QuotaFetchError {
    match error {
        CcError::UpstreamHttp { status, code, body } => {
            QuotaFetchError::Http { status, code, body }
        }
        CcError::Timeout(limit_ms) => QuotaFetchError::Timeout(limit_ms),
        CcError::Protocol(message) => QuotaFetchError::Decode(message),
        other => QuotaFetchError::Transport(other.to_string()),
    }
}

/// 单个端点请求失败时记录的内容。
///
/// [EndpointResponses] 借用响应体，而请求一旦返回错误，响应体就已经被消费成
/// 这个结构体（见 [QuotaPoller::poll_account] 的说明），因此需要一个能把
/// 「错误」活到组装快照那一刻的载体。
#[derive(Debug, Clone)]
struct EndpointFailure {
    /// 稳定端点名（英文，供结构化日志）。
    name: &'static str,
    /// 失败原因。
    error: QuotaFetchError,
}

/// 一次配额更新：某个账号的最新快照。
#[derive(Debug, Clone)]
pub struct QuotaUpdate {
    /// 账号槽位 id。
    pub account_id: String,
    /// 该账号的配额快照（只读结果）。
    pub snapshot: QuotaSnapshot,
    /// 快照生成时刻（epoch 毫秒）；由注入的时钟决定。
    pub at_ms: i64,
}

/// 一个账号的探测结果。
enum PollOutcome {
    /// 四个端点都探测过（可能有部分失败）。
    Done {
        /// whoami 的响应体；未发起或已失败时为 None。
        whoami: Option<Value>,
        /// billing/credits 的响应体。
        credits: Option<Value>,
        /// billing/subscriptions 的响应体。
        subscriptions: Option<Value>,
        /// usage/summary 的响应体。
        usage_summary: Option<Value>,
        /// 失败端点列表。
        failures: Vec<EndpointFailure>,
    },
    /// whoami 401/403：停止探测剩余端点。
    ShortCircuited(QuotaFetchError),
}

/// 时钟函数：返回 epoch 毫秒（Unix epoch）。
///
/// 抽成类型别名是为了让 [QuotaPoller::new_with_clock] 的签名可读；
/// 生产用 [crate::upstream::now_ms]，测试注入固定值。
pub type ClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 配额轮询器。
///
/// 典型用法（伪代码）：
///
/// ```ignore
/// let poller = Arc::new(QuotaPoller::new(config, upstream, interval));
/// poller.set_accounts(&slots, &resolve_key);
/// let mut rx = poller.subscribe();
/// tokio::spawn(Arc::clone(&poller).run(shutdown_rx));
/// let snapshots = poller.poll_once().await;
/// ```
///
/// 线程模型：[QuotaPoller::run] 是逐轮的「睡眠 → 轮询」；睡眠期间收到 shutdown
/// 会立刻醒来退出，轮询期间收到 shutdown 也会在当前账号结束后退出——每个账号
/// 都有 [QuotaPoller::with_timeout] 的上限，因此不会挂死。
pub struct QuotaPoller {
    /// 上游 HTTP 客户端。
    ///
    /// 配额端点与生成通道是同一个上游面，两个句柄放在一起是为了让
    /// `api_base` 的漂移在构造时就能被发现（不一致会打 warn）。
    upstream: Arc<UpstreamClient>,
    /// 轮询间隔。
    interval: Duration,
    /// 单次轮询（一个账号的四个端点）的整体耗时上限。
    timeout: Duration,
    /// 要轮的账号列表（槽位 + key）。
    ///
    /// 用 [RwLock] 而不是 [Mutex]：运行期账号增删是低频操作，读侧（下一轮该轮谁）
    /// 远比写侧频繁。这里与 tokio 无关，因此用标准库锁。
    accounts: RwLock<Vec<PollAccount>>,
    /// 更新广播通道。即便当前没有订阅者也不会丢数据——只是没人接收而已。
    updates: broadcast::Sender<QuotaUpdate>,
    /// 更新时间戳与快照 `now_ms` 的时钟。可注入是为了让测试完全确定。
    clock: ClockFn,
}

/// 一个待轮询账号：槽位事实 + 解析出的 key。
///
/// 带完整 [AccountSlot]（而不仅是 id）是为了让日志与面板能显示展示名，
/// 同时避免本模块反向依赖代理层的槽位解析流程。
struct PollAccount {
    /// 槽位（id + 展示名）。见 [crate::pool::AccountSlot]。
    slot: AccountSlot,
    /// 明文 API key：只用于发请求，绝不进日志、绝不进快照。
    key: String,
}

impl QuotaPoller {
    /// 用配置、上游客户端与轮询间隔构造轮询器。
    ///
    /// `interval` 由调用方注入而不是写死：生产取 [DEFAULT_INTERVAL_MS]，
    /// 测试取 50ms 这类很短的值，两者走同一条代码路径。
    pub fn new(config: Config, upstream: Arc<UpstreamClient>, interval: Duration) -> Self {
        Self::new_with_clock(config, upstream, interval, Arc::new(now_ms))
    }

    /// 同 [QuotaPoller::new]，但注入时钟。
    ///
    /// `clock` 同时决定 [QuotaUpdate::at_ms] 与传给
    /// [crate::quota::build_snapshot] 的 `now_ms`，因此「快照时刻」在测试里是
    /// 确定值（docs/STYLE.md 第 2.3 节：库代码内部不读系统时间）。
    pub fn new_with_clock(
        config: Config,
        upstream: Arc<UpstreamClient>,
        interval: Duration,
        clock: ClockFn,
    ) -> Self {
        let (updates, _no_subscriber) = broadcast::channel(UPDATE_CHANNEL_CAPACITY);
        // 两处配置漂移会让「面板显示的额度」与「实际发请求的账号」对不上，
        // 是极难排查的一类问题，因此在构造时就对照一次。
        if config.api_base != upstream.config().api_base {
            tracing::warn!(
                poller_api_base = %config.api_base,
                upstream_api_base = %upstream.config().api_base,
                "配额轮询器与上游客户端的 api_base 不一致：实际请求使用上游客户端的配置"
            );
        }
        Self {
            upstream,
            interval,
            timeout: Duration::from_millis(DEFAULT_POLL_TIMEOUT_MS),
            accounts: RwLock::new(Vec::new()),
            updates,
            clock,
        }
    }

    /// 设置单次轮询的整体耗时上限。
    ///
    /// 测试常把它设得很短（几毫秒），用来验证「上游挂死不会让循环停摆」。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 当前轮询间隔。
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// 构造时传入的上游客户端句柄。
    ///
    /// 宿主可以用它确认配额轮询与生成通道打的是同一个上游；本模块自身只读，
    /// 不会通过它发起生成请求（配额探测是纯只读的旁观者）。
    pub fn upstream(&self) -> &Arc<UpstreamClient> {
        &self.upstream
    }

    /// 整体替换要轮询的账号列表。
    ///
    /// `resolve_key` 与代理层的 [crate::proxy::KeyResolver] 是同一个函数——
    /// key 的存放位置是宿主的事实，本模块不关心。
    ///
    /// 解析不出 key 的槽位**被丢弃而不是每轮报 401**：它本来就没法发请求，
    /// 留着只会在每一轮产生一条无意义的「密钥被拒」。这里同时给出一行 debug 日志。
    pub fn set_accounts(&self, slots: &[AccountSlot], resolve_key: &crate::proxy::KeyResolver) {
        let accounts: Vec<PollAccount> = slots
            .iter()
            .filter_map(|slot| match resolve_key(slot) {
                Some(key) if !key.is_empty() => Some(PollAccount {
                    slot: slot.clone(),
                    key,
                }),
                _ => {
                    // 日志里**不得**出现 key 本身（docs/STYLE.md 第 5 节红线）。
                    tracing::debug!(account_id = %slot.id, "槽位没有可用的 key，跳过配额轮询");
                    None
                }
            })
            .collect();
        tracing::debug!(count = accounts.len(), "已更新待轮询账号列表");
        self.replace_accounts(accounts);
    }

    /// 只更新一个账号的 key（增删账号后不必重建整个列表时用）。
    ///
    /// 槽位不在当前列表里则追加；`key` 为 None 或空串表示该账号暂时没有凭据，
    /// 此时**移除**它而不是留一个每轮都会失败的空壳。
    pub fn upsert_account(&self, slot: &AccountSlot, key: Option<String>) {
        let mut guard = self
            .accounts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = guard.iter().position(|account| account.slot.id == slot.id);
        match key.filter(|key| !key.is_empty()) {
            Some(key) => {
                let account = PollAccount {
                    slot: slot.clone(),
                    key,
                };
                match existing {
                    Some(index) => guard[index] = account,
                    None => guard.push(account),
                }
            }
            None => {
                if let Some(index) = existing {
                    guard.remove(index);
                }
            }
        }
    }

    /// 当前待轮询的账号 id 列表（顺序即轮询顺序）。
    ///
    /// 只暴露 id：key 绝不出本模块（docs/STYLE.md 第 5 节）。
    pub fn account_ids(&self) -> Vec<String> {
        self.read_accounts()
            .iter()
            .map(|account| account.slot.id.clone())
            .collect()
    }

    /// 订阅配额更新。
    ///
    /// 通道是**最新值优先**的广播：订阅者处理不过来时会丢掉旧快照
    /// （[broadcast::error::RecvError::Lagged]），而不是把轮询器堵住。
    pub fn subscribe(&self) -> broadcast::Receiver<QuotaUpdate> {
        self.updates.subscribe()
    }

    /// 当前订阅者数量（诊断用）。
    pub fn subscriber_count(&self) -> usize {
        self.updates.receiver_count()
    }

    /// 轮询一遍所有账号，返回 `(账号 id, 快照)` 列表（顺序与 [Self::account_ids] 一致）。
    ///
    /// 单个账号失败不会中断整轮：每个账号各自产出自己的快照，失败信息进
    /// [crate::quota::QuotaSnapshot::last_error]；结果同时广播给订阅者。
    /// **失败不改变任何账号的可用状态**——本模块不接触账号池（docs/PROTOCOL.md 第 7 节）。
    pub async fn poll_once(&self) -> Vec<(String, QuotaSnapshot)> {
        // 先克隆出本轮要轮的账号：探测期间账号列表可能被改（增删账号），
        // 不能抱着读锁做网络请求。
        let accounts: Vec<(String, AccountSlot, String)> = self
            .read_accounts()
            .iter()
            .map(|account| {
                (
                    account.slot.id.clone(),
                    account.slot.clone(),
                    account.key.clone(),
                )
            })
            .collect();

        let mut results = Vec::with_capacity(accounts.len());
        for (account_id, slot, key) in accounts {
            let at_ms = (self.clock)();
            let outcome = self.poll_account(&key).await;
            let snapshot = snapshot_from(outcome, at_ms);
            self.publish(QuotaUpdate {
                account_id: account_id.clone(),
                snapshot: snapshot.clone(),
                at_ms,
            });
            tracing::debug!(
                account_id = %slot.id,
                has_error = snapshot.last_error.is_some(),
                alerts = snapshot.alerts.len(),
                "配额轮询完成"
            );
            results.push((account_id, snapshot));
        }
        results
    }

    /// 后台循环：立即轮询一次，此后每个间隔一次，直到 `shutdown` 置位。
    ///
    /// **开跑先探一次**：面板不该等满一个周期才第一次有数据。
    ///
    /// 退出保证：睡眠期间用 [tokio::select] 监听 shutdown（立刻醒来）；轮询期间
    /// 每个账号都受 [QuotaPoller::with_timeout] 的上限约束，因此也不会挂死。
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        // 进入时已经是「要停」状态就不轮询：调用方可能在启动前就取消了。
        if *shutdown.borrow() {
            tracing::debug!("配额轮询器在启动前已收到 shutdown，直接退出");
            return;
        }
        loop {
            let updates = self.poll_once().await;
            tracing::debug!(accounts = updates.len(), "完成一轮配额轮询");

            // 定时器等，同时监听 shutdown；间隔内被唤醒即立刻退出。
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                changed = shutdown.changed() => {
                    // 发送端被 drop（is_err）或值变为 true 都表示该收摊了。
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }

    /// 推送给订阅者；没有订阅者时 send 会返回 Err，这是正常情况，不是错误。
    fn publish(&self, update: QuotaUpdate) {
        let _ = self.updates.send(update);
    }

    /// 读出账号快照；锁中毒时取回内部数据继续跑。
    ///
    /// 中毒意味着另一个线程在持锁时 panic 了——那不该让配额轮询彻底停摆，
    /// 面板显示旧数据总比整个功能消失要好。
    fn read_accounts(&self) -> std::sync::RwLockReadGuard<'_, Vec<PollAccount>> {
        self.accounts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 整体替换账号列表。
    fn replace_accounts(&self, accounts: Vec<PollAccount>) {
        let mut guard = self
            .accounts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = accounts;
    }

    /// 探测一个账号的四个端点。
    ///
    /// 顺序与参考实现完全一致：whoami → credits → subscriptions → usage/summary。
    /// 每个端点独立失败（whoami 的 401/403 除外，见 [QuotaFetchError::is_credential_rejected]）。
    async fn poll_account(&self, key: &str) -> PollOutcome {
        match self.fetch(key, QuotaEndpoint::Whoami.path()).await {
            Ok(whoami) => {
                // whoami 成功 → 顺便拿到 orgId 供订阅端点使用（参考实现同此）。
                let org_id = AccountIdentity::from_whoami(&whoami).org_id;
                let org_query = org_id.map(|id| format!("?orgId={id}"));
                self.fetch_remaining(key, Some(whoami), org_query, Vec::new())
                    .await
            }
            Err(error) if error.is_credential_rejected() => {
                // docs/PROTOCOL.md 第 7 节：密钥无效时没必要继续打其余端点。
                tracing::warn!(
                    endpoint = QuotaEndpoint::Whoami.name(),
                    status = error.status(),
                    "whoami 判定密钥无效，本轮跳过其余配额端点"
                );
                PollOutcome::ShortCircuited(error)
            }
            Err(error) => {
                let failures = vec![EndpointFailure {
                    name: QuotaEndpoint::Whoami.name(),
                    error,
                }];
                self.fetch_remaining(key, None, None, failures).await
            }
        }
    }

    /// 探测 whoami 之后的三个端点（whoami 已成功时传入其响应体）。
    async fn fetch_remaining(
        &self,
        key: &str,
        whoami: Option<Value>,
        org_query: Option<String>,
        mut failures: Vec<EndpointFailure>,
    ) -> PollOutcome {
        let credits = match self.fetch(key, QuotaEndpoint::Credits.path()).await {
            Ok(body) => Some(body),
            Err(error) => {
                failures.push(EndpointFailure {
                    name: QuotaEndpoint::Credits.name(),
                    error,
                });
                None
            }
        };

        // 订阅端点的路径带 ?orgId=。orgId 是 UUID 或数字，不含需要转义的字符，
        // 因此直接拼接等价于参考实现的 encodeURIComponent。
        let subscriptions_path = match org_query.as_deref() {
            Some(query) => format!("{}{query}", QuotaEndpoint::Subscriptions.path()),
            None => QuotaEndpoint::Subscriptions.path().to_string(),
        };
        let subscriptions = match self.fetch(key, &subscriptions_path).await {
            Ok(body) => Some(body),
            Err(error) => {
                failures.push(EndpointFailure {
                    name: QuotaEndpoint::Subscriptions.name(),
                    error,
                });
                None
            }
        };

        let usage_summary = match self.fetch(key, QuotaEndpoint::UsageSummary.path()).await {
            Ok(body) => Some(body),
            Err(error) => {
                failures.push(EndpointFailure {
                    name: QuotaEndpoint::UsageSummary.name(),
                    error,
                });
                None
            }
        };

        PollOutcome::Done {
            whoami,
            credits,
            subscriptions,
            usage_summary,
            failures,
        }
    }

    /// 发一次 GET 请求并解析 JSON；整体受 [QuotaPoller::with_timeout] 约束。
    ///
    /// **复用 [UpstreamClient::get_account_json]** 而不是自建 HTTP 客户端：
    /// 账户端点的请求头、错误分类与 body 截断必须只有一处实现（见该方法文档）。
    /// 本函数只负责把它的 [CcError] 翻译成本模块的错误类型，从而让「某个端点
    /// 失败不影响其余端点」的独立降级逻辑保持在本层。
    async fn fetch(&self, key: &str, path: &str) -> Result<Value, QuotaFetchError> {
        let call = self.upstream.get_account_json(key, path);
        match tokio::time::timeout(self.timeout, call).await {
            Err(_) => Err(QuotaFetchError::Timeout(self.timeout.as_millis() as u64)),
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(map_cc_error(error)),
        }
    }
}

/// 把一个探测结果组装成 [QuotaSnapshot]。
///
/// 这是**唯一**调用 [build_snapshot] 的地方；whoami 短路时不调用它——身份为空、
/// `last_error` 记下被拒原因即可，解析工作不必也无从进行。
fn snapshot_from(outcome: PollOutcome, now_ms: i64) -> QuotaSnapshot {
    match outcome {
        PollOutcome::Done {
            whoami,
            credits,
            subscriptions,
            usage_summary,
            failures,
        } => {
            // 失败原因先固化成 String：EndpointResponses 借用响应体，而错误信息是
            // 临时构造的，不能在闭包里返回它的 &str。
            let messages: HashMap<&str, String> = failures
                .iter()
                .map(|failure| (failure.name, failure.error.to_string()))
                .collect();
            build_snapshot(
                EndpointResponses {
                    whoami: endpoint_result(&whoami, QuotaEndpoint::Whoami.name(), &messages),
                    credits: endpoint_result(&credits, QuotaEndpoint::Credits.name(), &messages),
                    subscriptions: endpoint_result(
                        &subscriptions,
                        QuotaEndpoint::Subscriptions.name(),
                        &messages,
                    ),
                    usage_summary: endpoint_result(
                        &usage_summary,
                        QuotaEndpoint::UsageSummary.name(),
                        &messages,
                    ),
                },
                now_ms,
            )
        }
        PollOutcome::ShortCircuited(error) => QuotaSnapshot {
            // 面向用户的是 user_message（带「怎么办」），诊断原文留给日志。
            last_error: Some(error.user_message()),
            ..QuotaSnapshot::default()
        },
    }
}

/// 组装一个端点在 [EndpointResponses] 里的槽位。
///
/// - 有响应体 → `Ok(&Value)`；
/// - 请求过但失败 → `Err(失败原因)`（由 [build_snapshot] 汇总进 last_error）；
/// - 根本没发起（whoami 短路）→ None。
fn endpoint_result<'body, 'reason>(
    body: &'body Option<Value>,
    name: &str,
    messages: &'reason HashMap<&'reason str, String>,
) -> Option<Result<&'body Value, &'reason str>> {
    match body {
        Some(value) => Some(Ok(value)),
        None => messages.get(name).map(|message| Err(message.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_upstream::{
        MockQuotaResponse, MockResponse, MockUpstream, CREDITS_PATH, SUBSCRIPTIONS_PATH,
        USAGE_SUMMARY_PATH, WHOAMI_PATH,
    };
    use serde_json::json;

    /// 构造一个指向 mock 上游的配置。
    ///
    /// 把请求超时调短，是为了让「上游挂死」的用例快速失败而不是真的等 60s。
    fn config_for(base: &str) -> Config {
        Config {
            api_base: base.to_string(),
            request_timeout: Duration::from_secs(5),
            ..Config::default()
        }
    }

    /// 固定时钟：让 at_ms / now_ms 在断言里是确定值。
    fn fixed_clock(at_ms: i64) -> ClockFn {
        Arc::new(move || at_ms)
    }

    /// 两个账号：default / second。
    fn two_slots() -> Vec<AccountSlot> {
        vec![
            AccountSlot {
                id: "default".into(),
                label: "默认".into(),
            },
            AccountSlot {
                id: "second".into(),
                label: "备用".into(),
            },
        ]
    }

    /// 两个槽位分别解析出不同的 key。
    fn resolver() -> crate::proxy::KeyResolver {
        Arc::new(|slot: &AccountSlot| Some(format!("key-{}", slot.id)))
    }

    /// mock 的生成脚本为空即可：配额用例只打 /alpha/* 端点。
    fn quota_mock() -> impl std::future::Future<Output = MockUpstream> {
        MockUpstream::start(Vec::<MockResponse>::new())
    }

    /// 造一个已注册两个账号的轮询器。
    fn poller_for(base: &str, interval: Duration, clock: ClockFn) -> Arc<QuotaPoller> {
        let config = config_for(base);
        let upstream = Arc::new(UpstreamClient::new(config.clone()).expect("测试客户端应能构造"));
        let poller = Arc::new(QuotaPoller::new_with_clock(
            config, upstream, interval, clock,
        ));
        poller.set_accounts(&two_slots(), &resolver());
        poller
    }

    /// 自定义 credits 响应体；用来验证两个账号拿到的是各自的数据。
    fn credits_json(monthly: f64) -> String {
        json!({
            "credits": { "monthlyCredits": monthly, "purchasedCredits": 0, "freeCredits": 0 },
            "windowLimits": { "fiveHour": { "used": 1, "cap": 10 } },
            "belowThreshold": false
        })
        .to_string()
    }

    #[tokio::test]
    async fn normal_poll_returns_a_snapshot_for_every_account() {
        let mock = quota_mock().await;
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(77));

        let results = poller.poll_once().await;

        assert_eq!(results.len(), 2, "两个账号都应有结果");
        assert_eq!(results[0].0, "default", "结果顺序应与账号列表一致");
        assert_eq!(results[1].0, "second");
        for (account_id, snapshot) in &results {
            assert_eq!(
                snapshot.identity.user_name.as_deref(),
                Some("tester"),
                "whoami 应被解析进身份（{account_id}）"
            );
            assert_eq!(
                snapshot.credits.as_ref().map(|credits| credits.monthly),
                Some(20.0),
                "credits 端点的余额应被解析（{account_id}）"
            );
            assert_eq!(
                snapshot.plan_id.as_deref(),
                Some("individual-go"),
                "套餐应来自订阅端点"
            );
            assert!(
                snapshot.alerts.is_empty(),
                "这个夹具没有任何告警，不应凭空出现告警"
            );
            assert!(
                snapshot.last_error.is_none(),
                "全部端点成功时不该记录失败原因"
            );
        }
        assert_eq!(
            mock.request_count(),
            8,
            "两个账号各打四个配额端点，共 8 次请求"
        );
    }

    #[tokio::test]
    async fn one_failing_endpoint_keeps_the_others_reporting() {
        // docs/PROTOCOL.md 第 7 节：某个端点失败不影响其余端点。
        let mock = quota_mock().await;
        assert!(
            mock.set_quota_script(
                CREDITS_PATH,
                vec![MockQuotaResponse::Json {
                    status: 500,
                    body: r#"{"error":{"code":"internal_error"}}"#.to_string(),
                }],
            ),
            "credits 是四个配额端点之一，脚本应被接受"
        );
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));

        let results = poller.poll_once().await;
        let (_, snapshot) = &results[0];

        assert!(
            snapshot.credits.is_none(),
            "credits 端点失败时余额应为 None（降级而不是 panic）"
        );
        assert_eq!(
            snapshot.identity.user_name.as_deref(),
            Some("tester"),
            "credits 失败不得影响 whoami 的数据"
        );
        assert_eq!(
            snapshot.plan_id.as_deref(),
            Some("individual-go"),
            "credits 失败也不得影响订阅端点"
        );
        let last_error = snapshot.last_error.as_deref().unwrap_or_default();
        assert!(
            last_error.contains("billing/credits"),
            "失败原因应汇总进 last_error，实际 {last_error}"
        );
    }

    #[tokio::test]
    async fn whoami_401_short_circuits_the_remaining_endpoints() {
        let mock = quota_mock().await;
        assert!(
            mock.set_quota_script(
                WHOAMI_PATH,
                vec![MockQuotaResponse::Json {
                    status: 401,
                    body: r#"{"error":{"code":"unauthorized"}}"#.to_string(),
                }],
            ),
            "whoami 是四个配额端点之一，脚本应被接受"
        );
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));

        let results = poller.poll_once().await;

        assert_eq!(results.len(), 2, "每个账号仍应产出结果");
        for (account_id, snapshot) in &results {
            let last_error = snapshot.last_error.as_deref().unwrap_or_default();
            assert!(
                last_error.contains("密钥被拒"),
                "用户文案应说明密钥被拒（{account_id}），实际 {last_error}"
            );
            assert!(
                snapshot.credits.is_none() && snapshot.plan_id.is_none(),
                "短路后不应有其余端点的数据（{account_id}）"
            );
        }
        assert_eq!(
            mock.request_count(),
            2,
            "whoami 401 后不得再打其余三个端点（每个账号只应发 1 次）"
        );
        assert!(
            mock.requests()
                .iter()
                .all(|request| request.path == WHOAMI_PATH),
            "短路轮只应命中 whoami 端点"
        );
    }

    #[tokio::test]
    async fn accounts_do_not_share_data() {
        let mock = quota_mock().await;
        assert!(
            mock.set_quota_script(
                CREDITS_PATH,
                vec![
                    MockQuotaResponse::Json {
                        status: 200,
                        body: credits_json(20.0),
                    },
                    MockQuotaResponse::Json {
                        status: 200,
                        body: credits_json(99.0),
                    },
                ],
            ),
            "credits 脚本应被接受"
        );
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));

        let results = poller.poll_once().await;

        assert_eq!(
            results[0].1.credits.as_ref().map(|credits| credits.monthly),
            Some(20.0),
            "default 账号拿到的是它自己那一次响应的余额"
        );
        assert_eq!(
            results[1].1.credits.as_ref().map(|credits| credits.monthly),
            Some(99.0),
            "second 账号拿到的是它自己那一次响应的余额，不得串用 default 的数据"
        );
        assert_eq!(
            results[1].1.identity.user_name.as_deref(),
            Some("tester"),
            "每个账号的身份来自各自的 whoami 请求"
        );
    }

    #[tokio::test]
    async fn each_account_is_probed_with_its_own_key() {
        let mock = quota_mock().await;
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));

        let _ = poller.poll_once().await;

        let requests = mock.requests();
        assert_eq!(
            requests[0].bearer_token(),
            Some("key-default"),
            "第一个账号应使用自己的 key"
        );
        assert_eq!(
            requests[4].bearer_token(),
            Some("key-second"),
            "第二个账号应使用自己的 key，而不是复用前一个"
        );
    }

    #[tokio::test]
    async fn subscribers_receive_every_account_update() {
        let mock = quota_mock().await;
        let poller = poller_for(
            mock.base_url(),
            Duration::from_millis(50),
            fixed_clock(1234),
        );
        let mut updates = poller.subscribe();

        let _ = poller.poll_once().await;

        let first = updates.recv().await.expect("应收到第一个账号的更新");
        let second = updates.recv().await.expect("应收到第二个账号的更新");
        assert_eq!(first.account_id, "default");
        assert_eq!(second.account_id, "second");
        assert_eq!(
            first.at_ms, 1234,
            "时间戳必须来自注入的时钟（STYLE.md 2.3）"
        );
        assert_eq!(
            first
                .snapshot
                .credits
                .as_ref()
                .map(|credits| credits.monthly),
            Some(20.0),
            "订阅者拿到的应是完整快照"
        );
    }

    #[tokio::test]
    async fn shutdown_stops_the_run_loop() {
        let mock = quota_mock().await;
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let task = tokio::spawn(Arc::clone(&poller).run(shutdown_rx));
        // 等它至少跑完一轮，确保循环确实在工作
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!task.is_finished(), "shutdown 之前后台循环不应自行退出");

        shutdown_tx.send(true).expect("shutdown 通道应仍存活");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shutdown 后 run() 应立即退出，不得挂死")
            .expect("run() 不应 panic");
    }

    #[tokio::test]
    async fn dropping_the_shutdown_sender_also_stops_the_run_loop() {
        let mock = quota_mock().await;
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let task = tokio::spawn(Arc::clone(&poller).run(shutdown_rx));
        tokio::time::sleep(Duration::from_millis(60)).await;
        // 宿主进程收摊时发送端会随结构体一起 drop，这同样必须让循环退出。
        drop(shutdown_tx);

        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shutdown 通道被关闭后 run() 应退出，不得挂死")
            .expect("run() 不应 panic");
    }

    #[tokio::test]
    async fn run_returns_immediately_when_shutdown_is_already_set() {
        let mock = quota_mock().await;
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));
        let (_shutdown_tx, shutdown_rx) = watch::channel(true);

        tokio::time::timeout(Duration::from_secs(2), Arc::clone(&poller).run(shutdown_rx))
            .await
            .expect("启动前已置位的 shutdown 应让它立刻退出");

        assert_eq!(mock.request_count(), 0, "已经要求停止时不应再发起任何探测");
    }

    #[tokio::test]
    async fn slots_without_a_key_are_skipped() {
        let mock = quota_mock().await;
        let config = config_for(mock.base_url());
        let upstream = Arc::new(UpstreamClient::new(config.clone()).expect("测试客户端应能构造"));
        let poller = QuotaPoller::new_with_clock(
            config,
            upstream,
            Duration::from_millis(50),
            fixed_clock(0),
        );
        let only_first: crate::proxy::KeyResolver = Arc::new(|slot: &AccountSlot| {
            (slot.id == "default").then(|| "key-default".to_string())
        });
        poller.set_accounts(&two_slots(), &only_first);

        let results = poller.poll_once().await;

        assert_eq!(
            poller.account_ids(),
            vec!["default".to_string()],
            "解析不出 key 的槽位不该进入轮询列表"
        );
        assert_eq!(results.len(), 1, "只应有一个账号的结果");
    }

    #[tokio::test]
    async fn subscriptions_endpoint_carries_the_org_id_from_whoami() {
        let mock = quota_mock().await;
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));

        let _ = poller.poll_once().await;

        let target = mock
            .requests()
            .into_iter()
            .find(|request| request.path == SUBSCRIPTIONS_PATH)
            .map(|request| request.target);
        let target = target.expect("订阅端点必须被请求过");
        assert!(
            target.contains("orgId="),
            "订阅端点应带上 whoami 的 orgId（参考实现 refreshAccount 的做法），实际 {target}"
        );
    }

    #[tokio::test]
    async fn a_hanging_endpoint_times_out_without_stalling_the_loop() {
        let mock = quota_mock().await;
        // whoami 挂住不回应：没有整体超时的话这里会一直等下去。
        assert!(
            mock.set_quota_script(WHOAMI_PATH, vec![MockQuotaResponse::Hang]),
            "whoami 脚本应被接受"
        );
        // with_timeout 是消费式 builder，所以要在套上 Arc 之前调用。
        let config = config_for(mock.base_url());
        let upstream = Arc::new(UpstreamClient::new(config.clone()).expect("测试客户端应能构造"));
        let poller = Arc::new(
            QuotaPoller::new_with_clock(
                config,
                upstream,
                Duration::from_millis(50),
                fixed_clock(0),
            )
            .with_timeout(Duration::from_millis(20)),
        );
        poller.set_accounts(&two_slots(), &resolver());

        let results = tokio::time::timeout(Duration::from_secs(2), poller.poll_once())
            .await
            .expect("有超时上限时 poll_once 必须能返回，不得挂死");

        assert_eq!(results.len(), 2, "超时也应给每个账号产出一条结果");
        for (account_id, snapshot) in &results {
            let last_error = snapshot.last_error.as_deref().unwrap_or_default();
            assert!(
                last_error.contains("whoami"),
                "挂住的端点应作为失败被记进 last_error（{account_id}），实际 {last_error}"
            );
        }
    }

    #[tokio::test]
    async fn truncated_error_body_is_kept_for_diagnostics() {
        let mock = quota_mock().await;
        assert!(
            mock.set_quota_script(
                WHOAMI_PATH,
                vec![MockQuotaResponse::Json {
                    status: 500,
                    body: "x".repeat(2_000),
                }],
            ),
            "whoami 脚本应被接受"
        );
        let poller = poller_for(mock.base_url(), Duration::from_millis(50), fixed_clock(0));

        let results = poller.poll_once().await;
        let last_error = results[0].1.last_error.as_deref().unwrap_or_default();
        assert!(
            last_error.chars().count() < 2_000,
            "错误体必须截断保存，不能把整个 HTML 错误页塞进快照，实际长度 {}",
            last_error.chars().count()
        );
        // 其余三个端点仍然被探测过：一个端点失败不等于整个账号失败。
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|request| request.path == USAGE_SUMMARY_PATH)
                .count(),
            2,
            "whoami 失败后 usage/summary 仍应被探测（每账号一次）"
        );
    }

    #[test]
    fn credential_rejection_covers_401_and_403_only() {
        assert!(
            QuotaFetchError::Http {
                status: 401,
                code: None,
                body: String::new()
            }
            .is_credential_rejected(),
            "401 是密钥无效"
        );
        assert!(
            QuotaFetchError::Http {
                status: 403,
                code: None,
                body: String::new()
            }
            .is_credential_rejected(),
            "403 同样说明该 key 无权访问配额端点"
        );
        assert!(
            !QuotaFetchError::Http {
                status: 500,
                code: None,
                body: String::new()
            }
            .is_credential_rejected(),
            "500 是上游故障，不该被当成密钥问题"
        );
        assert!(
            !QuotaFetchError::Transport("reset".into()).is_credential_rejected(),
            "网络失败与密钥无关"
        );
    }

    #[test]
    fn user_message_has_no_plaintext_key() {
        // docs/STYLE.md 第 5 节红线：面向用户的文案里绝不能出现明文 key。
        let message = QuotaFetchError::Http {
            status: 401,
            code: Some("unauthorized".into()),
            body: "Bearer user_secret-abcd".into(),
        }
        .user_message();
        assert!(
            !message.contains("user_secret"),
            "错误文案不得回显密钥，实际 {message}"
        );
    }

    #[test]
    fn cc_errors_keep_their_semantics_when_mapped() {
        assert!(
            map_cc_error(CcError::UpstreamHttp {
                status: 401,
                code: Some("unauthorized".into()),
                body: "{}".into(),
            })
            .is_credential_rejected(),
            "上游 401 映射后必须仍然可被识别为密钥被拒（短路依赖它）"
        );
        assert_eq!(
            map_cc_error(CcError::Protocol("bad json".into())),
            QuotaFetchError::Decode("bad json".into()),
            "解析失败应映射为 Decode，而不是混进传输错误"
        );
    }
}
