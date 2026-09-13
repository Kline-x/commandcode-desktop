//! 后台服务编排：打开存储、起控制面与代理面、注入前端。
//!
//! 这里是「外壳」与「核心」的接缝：cc-server 不认识 Tauri，本模块负责把
//! 平台相关的东西（数据目录、密钥、端口）交给它。

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use cc_server::control::{control_router, ControlState};
use cc_server::proxy::{router as proxy_router, KeyResolver, ProxyState};
use cc_server::store::Store;
use cc_server::{Config, UpstreamProtocol};
use tauri::AppHandle;
use tauri::Manager;

use crate::AppState;

/// 启动失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// 数据目录不可用。
    #[error("无法准备数据目录：{0}")]
    DataDir(String),
    /// 数据库打不开。
    #[error("无法打开本地数据库：{0}")]
    Database(String),
    /// 端口被占用（另一个实例在跑？）。
    #[error("无法监听 {addr}：{reason}")]
    Bind {
        /// 试图绑定的地址。
        addr: SocketAddr,
        /// 底层错误文本。
        ///
        /// 字段名**不能叫 source**：thiserror 会把 source 当作错误链上游，
        /// 要求它实现 Error，而这里只有一条文本（std::io::Error 本身不可 Clone）。
        reason: String,
    },
    /// 核心初始化失败。
    #[error("核心初始化失败：{0}")]
    Core(String),
}

/// 账号池与轮询器的热重载器。
#[derive(Clone)]
pub struct AccountReloader {
    store: Arc<Store>,
    secrets: Arc<crate::secrets::Secrets>,
    proxy_state: Arc<ProxyState>,
    poller: Arc<cc_server::quota_poller::QuotaPoller>,
}

impl AccountReloader {
    pub fn new(
        store: Arc<Store>,
        secrets: Arc<crate::secrets::Secrets>,
        proxy_state: Arc<ProxyState>,
        poller: Arc<cc_server::quota_poller::QuotaPoller>,
    ) -> Self {
        Self {
            store,
            secrets,
            proxy_state,
            poller,
        }
    }

    /// 从数据库重新加载启用的账号、路由规则和解密密钥，并热更新代理池和配额轮询器。
    pub fn reload(&self) {
        let slots = load_slots(&self.store);
        let key_lookup = Arc::new(load_key_map(&self.store, &self.secrets));
        let resolve_key: KeyResolver = {
            let map = key_lookup.clone();
            Arc::new(move |slot: &cc_server::AccountSlot| map.get(&slot.id).cloned())
        };
        let rules = load_rules(&self.store);
        self.proxy_state
            .set_accounts(slots.clone(), Arc::clone(&resolve_key));
        self.proxy_state.set_rules(rules);
        self.poller.set_accounts(&slots, &resolve_key);
        tracing::info!(
            accounts = slots.len(),
            "账号池、路由规则与配额轮询器已完成热重载"
        );

        // 立即触发一轮探测，无需干等下一个周期
        let poller = Arc::clone(&self.poller);
        tauri::async_runtime::spawn(async move {
            poller.poll_once().await;
        });
    }
}

/// 从数据库读路由规则。
fn load_rules(store: &Store) -> Vec<cc_server::pool::ModelAccountRule> {
    store
        .list_route_rules()
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let models = serde_json::from_str::<Vec<String>>(&row.models_json).unwrap_or_default();
            cc_server::pool::ModelAccountRule {
                models,
                account: row.account_id,
            }
        })
        .collect()
}

/// 启动后台服务，返回给前端用的连接信息。
pub fn start(
    app: &AppHandle,
) -> Result<
    (
        AppState,
        Arc<Store>,
        Arc<crate::secrets::Secrets>,
        AccountReloader,
    ),
    StartupError,
> {
    let data_dir = data_dir(app)?;
    std::fs::create_dir_all(&data_dir).map_err(|e| StartupError::DataDir(e.to_string()))?;

    let crash_file = data_dir.join("crash.log");
    let _ = crate::CRASH_LOG_PATH.set(crash_file.clone());
    if crash_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&crash_file) {
            tracing::warn!(
                crash_log = %crash_file.display(),
                "检测到上次运行的崩溃日志 (crash.log)"
            );
            crate::get_log_buffer().push(
                "WARN",
                format!("[CRASH RECOVERY] 检测到上次运行崩溃日志:\n{content}"),
            );
        }
    }

    let db_path = data_dir.join("commandcode.db");
    // 保留最近 5000 条流水：足够面板回溯，又不会让数据库无限增长
    let store =
        Arc::new(Store::open(&db_path, 5_000).map_err(|e| StartupError::Database(e.to_string()))?);

    // 控制面用随机端口 + 随机 token（见模块文档）
    let control_token = random_token();
    let control_listener = bind_random()?;
    let control_addr = control_listener
        .local_addr()
        .map_err(|e| StartupError::Bind {
            addr: "127.0.0.1:0"
                .parse()
                .unwrap_or_else(|_| SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            reason: e.to_string(),
        })?;

    // 代理面：固定 127.0.0.1:3050（客户端要能预期这个地址）
    let proxy_listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 3050)).map_err(|e| StartupError::Bind {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 3050)),
            reason: e.to_string(),
        })?;
    let proxy_addr = proxy_listener
        .local_addr()
        .map_err(|e| StartupError::Bind {
            addr: "127.0.0.1:3050".parse().unwrap(),
            reason: e.to_string(),
        })?;

    // 从数据库读账号，构造 key 解析器。
    // 注意：**密文的解密在宿主层**（见 secrets.rs），本模块只做接线。
    let slots = load_slots(&store);
    // 主密钥在这里加载一次，之后由 AppState 与 IPC 命令共享
    let secrets = Arc::new(crate::secrets::Secrets::load_or_create(&data_dir));
    if secrets.source() == crate::secrets::MasterKeySource::Ephemeral {
        tracing::warn!("本次运行使用临时主密钥：重启后需重新添加账号");
    }
    let key_lookup = Arc::new(load_key_map(&store, &secrets));
    let resolve_key: KeyResolver = {
        let map = key_lookup.clone();
        Arc::new(move |slot: &cc_server::AccountSlot| map.get(&slot.id).cloned())
    };

    // ---- 请求落库：把代理的每次请求写进 SQLite，面板据此展示流水 ----
    //
    // observer 是同步回调（代理路径上不能 await 数据库），所以这里只做一次
    // 快速 insert；SQLite 单写者模型下每次请求一行，成本可忽略。
    let store_for_records = store.clone();
    let observer: cc_server::RequestObserver = Arc::new(move |record| {
        let row = cc_server::store::NewRequest {
            at_ms: record.at_ms,
            account_id: record.account_id.clone(),
            account_label: if record.account_label.is_empty() {
                None
            } else {
                Some(record.account_label.clone())
            },
            model: record.model.clone(),
            protocol: record.protocol.to_string(),
            client_protocol: record.client_protocol.to_string(),
            stream: record.stream,
            status: record.status,
            error_code: record.error_code.clone(),
            input_tokens: record.usage.input_tokens as i64,
            output_tokens: record.usage.output_tokens as i64,
            cached_tokens: record.usage.cached_input_tokens as i64,
            ttft_ms: record.ttft_ms,
            total_ms: record.total_ms,
            attempts: record.attempts as i64,
            cost_usd: record.cost_usd,
        };
        if let Err(error) = store_for_records.insert_request(&row) {
            // 落库失败不能影响已经完成的请求；记日志即可
            tracing::warn!(%error, "请求流水写入失败");
        }
    });

    let config = Config {
        api_base: "https://api.commandcode.ai".to_string(),
        // Auto：先 Provider API，遇 upgrade_required 降级到 CLI（Go 套餐可用）
        upstream_protocol: UpstreamProtocol::Auto,
        ..Config::default()
    };
    let rules = load_rules(&store);
    let proxy_state = Arc::new(
        ProxyState::new(config.clone(), slots.clone(), Arc::clone(&resolve_key))
            .map_err(|e| StartupError::Core(e.to_string()))?
            .with_rules(rules)
            // observer 必须先挂上：它负责把每次请求写进 SQLite（面板的流水来源）
            .with_observer(observer),
    );
    // ---- 配额轮询：周期拉各账号的用量窗口，供面板展示 ----
    let poller = Arc::new(cc_server::quota_poller::QuotaPoller::new(
        config.clone(),
        Arc::clone(&proxy_state.upstream),
        Duration::from_millis(cc_server::quota_poller::DEFAULT_INTERVAL_MS),
    ));
    poller.set_accounts(&slots, &resolve_key);

    let reloader = AccountReloader::new(
        store.clone(),
        secrets.clone(),
        proxy_state.clone(),
        poller.clone(),
    );

    let reloader_for_control = reloader.clone();
    let control_state = Arc::new(
        ControlState::new(store.clone(), control_token.clone())
            .with_api_base("https://api.commandcode.ai")
            .with_log_buffer(crate::get_log_buffer())
            .with_account_callback(Arc::new(move || {
                reloader_for_control.reload();
            })),
    );
    let control_app = control_router(control_state);
    let proxy_app = proxy_router(proxy_state);

    let control_url = format!("http://{control_addr}");
    let proxy_url = format!("http://{proxy_addr}");

    // 两个服务各自跑在 tokio 运行时上；生命周期跟随进程。
    //
    // ⚠️ 必须先把 std 监听器设为 **nonblocking** 再交给 tokio：tokio 的
    // TcpListener::from_std 不会替你改这个标志，而 axum::serve 内部是 async
    // accept；监听器仍是阻塞模式时，一次 accept 就会把运行时工作线程占住，
    // 表现为「端口在 LISTEN、TCP 连接能建立，但永远收不到响应」。
    // 这个 bug 只在真正运行二进制时才会暴露（单测里用的是 tokio 自己的监听器）。
    tauri::async_runtime::spawn(async move {
        // 转换必须发生在运行时**内部**：TcpListener::from_std 要向 tokio reactor
        // 注册，在 setup 钩子（主线程、无运行时上下文）里调用会 panic。
        match to_tokio_listener(control_listener, control_addr) {
            Ok(listener) => {
                if let Err(error) = axum::serve(listener, control_app).await {
                    tracing::error!(%error, "控制面退出");
                }
            }
            Err(error) => tracing::error!(%error, "控制面无法启动"),
        }
    });
    // 配额轮询：周期拉各账号的用量窗口（面板的数据来源）。
    // 用 watch 通道做停机信号——它适合表达「状态」，broadcast 适合「事件」。
    let (quota_shutdown, quota_shutdown_rx) = tokio::sync::watch::channel(false);
    let poller_task = Arc::clone(&poller);
    tauri::async_runtime::spawn(async move {
        poller_task.run(quota_shutdown_rx).await;
        tracing::debug!("配额轮询已停止");
    });

    // 配额更新回写：将轮询快照同步写入 SQLite accounts 表
    let mut quota_rx = poller.subscribe();
    let store_for_quota = store.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match quota_rx.recv().await {
                Ok(update) => {
                    if let Ok(id) = update.account_id.parse::<i64>() {
                        let quota_json = serde_json::to_string(&update.snapshot).ok();
                        let last_error = update.snapshot.last_error.as_deref();
                        if let Err(error) = store_for_quota.update_account_quota(
                            id,
                            quota_json.as_deref(),
                            last_error,
                            update.at_ms,
                        ) {
                            tracing::warn!(%error, id, "写入账号配额快照失败");
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "配额回写通道落后，跳过过旧消息");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    tauri::async_runtime::spawn(async move {
        match to_tokio_listener(proxy_listener, proxy_addr) {
            Ok(listener) => {
                if let Err(error) = axum::serve(listener, proxy_app).await {
                    tracing::error!(%error, "代理面退出");
                }
            }
            Err(error) => tracing::error!(%error, "代理面无法启动"),
        }
    });

    tracing::info!(%proxy_url, %control_url, "后台服务已启动");

    Ok((
        AppState {
            control_base_url: control_url,
            control_token,
            proxy_base_url: proxy_url,
            data_dir,
            quota_shutdown,
        },
        store,
        secrets,
        reloader,
    ))
}

/// 生成前端要执行的注入脚本。
///
/// 只注入地址与 token，不含任何密钥——密钥始终留在 Rust 侧。
pub fn init_script(base_url: &str, token: &str) -> String {
    format!(
        "window.__CC_CONTROL__ = {{ baseUrl: {}, token: {} }};",
        serde_json::to_string(base_url).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(token).unwrap_or_else(|_| "\"\"".into()),
    )
}

/// 解析（并在需要时创建）应用数据目录。
///
/// 用 Tauri 的路径解析：三平台各自落到正确位置（macOS 的 Application Support、
/// Windows 的 AppData、Linux 的 XDG 数据目录），无需自己判断平台。
fn data_dir(app: &AppHandle) -> Result<std::path::PathBuf, StartupError> {
    app.path()
        .app_data_dir()
        .map_err(|e| StartupError::DataDir(e.to_string()))
}

/// 绑定一个随机可用端口。
fn bind_random() -> Result<TcpListener, StartupError> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    TcpListener::bind(addr).map_err(|e| StartupError::Bind {
        addr,
        reason: e.to_string(),
    })
}

/// 生成 32 字节的随机 token（十六进制）。
///
/// 用系统随机源；token 只活在内存里，不落磁盘、不进日志。
fn random_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

/// 从数据库读出启用的账号，转成核心层的槽位。
fn load_slots(store: &Store) -> Vec<cc_server::AccountSlot> {
    match store.list_accounts() {
        Ok(rows) => rows
            .into_iter()
            .filter(|row| row.enabled)
            .map(|row| cc_server::AccountSlot {
                // 槽位 id 用数据库主键的字符串形式：稳定且与面板一致
                id: row.id.to_string(),
                label: row.label,
            })
            .collect(),
        Err(error) => {
            tracing::error!(%error, "读取账号失败，将以空池启动");
            Vec::new()
        }
    }
}

/// 构造槽位 id → API key 的映射。
///
/// 这里调用 secrets 解密；当前实现把「密文即明文」的占位逻辑留在 secrets.rs，
/// 接入 stronghold 后本函数无需改动。
fn load_key_map(
    store: &Store,
    secrets: &crate::secrets::Secrets,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    match store.list_accounts() {
        Ok(rows) => {
            for row in rows {
                if !row.enabled {
                    continue;
                }
                match secrets.decrypt(&row.key_cipher) {
                    Ok(key) => {
                        map.insert(row.id.to_string(), key);
                    }
                    Err(error) => {
                        tracing::warn!(account = %row.label, %error, "账号密钥解密失败，跳过该账号");
                    }
                }
            }
        }
        Err(error) => tracing::error!(%error, "读取账号失败"),
    }
    map
}

/// 把标准库监听器转成 tokio 监听器，并确保它是非阻塞的。
///
/// 见调用处的说明：漏掉 nonblocking 会让 async 运行时的工作线程被一次
/// 阻塞式 accept 占死，症状是端口在监听但没有任何响应。
fn to_tokio_listener(
    listener: TcpListener,
    addr: SocketAddr,
) -> Result<tokio::net::TcpListener, StartupError> {
    listener
        .set_nonblocking(true)
        .map_err(|e| StartupError::Bind {
            addr,
            reason: e.to_string(),
        })?;
    tokio::net::TcpListener::from_std(listener).map_err(|e| StartupError::Bind {
        addr,
        reason: e.to_string(),
    })
}
