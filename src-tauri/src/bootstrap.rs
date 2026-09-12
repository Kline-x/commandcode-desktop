//! 后台服务编排：打开存储、起控制面与代理面、注入前端。
//!
//! 这里是「外壳」与「核心」的接缝：cc-server 不认识 Tauri，本模块负责把
//! 平台相关的东西（数据目录、密钥、端口）交给它。

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;

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

/// 启动后台服务，返回给前端用的连接信息。
pub fn start(app: &AppHandle) -> Result<(AppState, Arc<Store>), StartupError> {
    let data_dir = data_dir(app)?;
    std::fs::create_dir_all(&data_dir).map_err(|e| StartupError::DataDir(e.to_string()))?;

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

    let control_state = Arc::new(ControlState::new(store.clone(), control_token.clone()));
    let control_app = control_router(control_state);

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
    let key_lookup = Arc::new(load_key_map(&store));
    let resolve_key: KeyResolver = {
        let map = key_lookup.clone();
        Arc::new(move |slot: &cc_server::AccountSlot| map.get(&slot.id).cloned())
    };

    let config = Config {
        api_base: "https://api.commandcode.ai".to_string(),
        // Auto：先 Provider API，遇 upgrade_required 降级到 CLI（Go 套餐可用）
        upstream_protocol: UpstreamProtocol::Auto,
        ..Config::default()
    };
    let proxy_state = Arc::new(
        ProxyState::new(config, slots, resolve_key)
            .map_err(|e| StartupError::Core(e.to_string()))?,
    );
    let proxy_app = proxy_router(proxy_state);

    let control_url = format!("http://{control_addr}");
    let proxy_url = format!("http://{proxy_addr}");

    // 两个服务各自跑在 tokio 运行时上；生命周期跟随进程
    tauri::async_runtime::spawn(async move {
        if let Err(error) = axum::serve(
            tokio::net::TcpListener::from_std(control_listener).unwrap(),
            control_app,
        )
        .await
        {
            tracing::error!(%error, "控制面退出");
        }
    });
    tauri::async_runtime::spawn(async move {
        if let Err(error) = axum::serve(
            tokio::net::TcpListener::from_std(proxy_listener).unwrap(),
            proxy_app,
        )
        .await
        {
            tracing::error!(%error, "代理面退出");
        }
    });

    tracing::info!(%proxy_url, %control_url, "后台服务已启动");

    Ok((
        AppState {
            control_base_url: control_url,
            control_token,
            proxy_base_url: proxy_url,
            data_dir,
        },
        store,
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
fn load_key_map(store: &Store) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    match store.list_accounts() {
        Ok(rows) => {
            for row in rows {
                if !row.enabled {
                    continue;
                }
                match crate::secrets::decrypt_key(&row.key_cipher) {
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
