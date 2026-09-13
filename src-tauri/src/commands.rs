//! Tauri IPC 命令：前端可直接调用的原生能力。
//!
//! 为什么需要它们（而不是全走控制面 HTTP）：
//! **密钥的加密只能发生在 Rust 侧**。前端拿到明文 key 后经 IPC 交给这里，
//! 由 [crate::secrets] 加密、落库，明文立刻离开前端内存。若让前端自己调控制面
//! 的 `/api/accounts`，那它就必须自己加密——等同于把加密逻辑和主密钥暴露给 WebView。

use tauri::{AppHandle, Manager, State};

use crate::secrets::Secrets;
use crate::AppState;

/// IPC 命令的错误：序列化成前端可读的字符串。
#[derive(Debug, serde::Serialize)]
pub struct CommandError {
    /// 错误消息（中文，面向用户）。
    pub message: String,
}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

/// 新增一个账号：加密 key → 落库 → 返回新账号 id 与提示。
///
/// 明文 key 只在本函数栈上存在，加密后即丢弃；返回值里**只有提示**。
#[tauri::command]
pub async fn add_account(
    app: AppHandle,
    label: String,
    api_key: String,
) -> Result<AddAccountResult, CommandError> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err(CommandError::from("账号名称不能为空".to_string()));
    }
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(CommandError::from("API key 不能为空".to_string()));
    }

    let store = store_of(&app)?;
    // 主密钥由 bootstrap 在启动时加载并 manage，这里只借用
    let secrets = app
        .try_state::<SecretsHandle>()
        .ok_or_else(|| CommandError::from("密钥服务尚未就绪".to_string()))?;

    let hint = crate::secrets::key_hint(&api_key);
    let cipher = secrets
        .0
        .encrypt(&api_key)
        .map_err(|e| CommandError::from(e.to_string()))?;

    // 重复检测：同一 key 的提示相同
    match store.find_account_by_hint(&hint) {
        Ok(Some(existing)) => {
            return Err(CommandError::from(format!(
                "该密钥已存在（{}）",
                existing.label
            )));
        }
        Ok(None) => {}
        Err(e) => return Err(CommandError::from(e.to_string())),
    }

    let created_at_ms = cc_server::now_ms();
    let id = store
        .insert_account(&label, &cipher, &hint, created_at_ms)
        .map_err(|e| CommandError::from(e.to_string()))?;

    tracing::info!(account_id = id, %label, key_hint = %hint, "账号已添加");
    Ok(AddAccountResult { id, key_hint: hint })
}

/// 新增账号的返回。
#[derive(Debug, serde::Serialize)]
pub struct AddAccountResult {
    /// 新账号 id。
    pub id: i64,
    /// 密钥提示（形如 `user_…ab12`）。
    pub key_hint: String,
}

/// 让面板能拿到控制面地址与 token（前端从 window 注入失败时的兜底）。
#[tauri::command]
pub fn control_endpoint(state: State<'_, AppState>) -> ControlEndpoint {
    ControlEndpoint {
        base_url: state.control_base_url.clone(),
        token: state.control_token.clone(),
        proxy_base_url: state.proxy_base_url.clone(),
    }
}

/// 控制面连接信息。
#[derive(Debug, serde::Serialize)]
pub struct ControlEndpoint {
    /// 控制面基址。
    pub base_url: String,
    /// 控制面 token。
    pub token: String,
    /// 代理面基址（展示给用户填进客户端）。
    pub proxy_base_url: String,
}

/// 打开数据目录（托盘与设置页用）。
#[tauri::command]
pub fn reveal_data_dir(state: State<'_, AppState>) -> Result<(), CommandError> {
    let dir = state.data_dir.display().to_string();
    tracing::info!(data_dir = %dir, "数据目录");
    Ok(())
}

/// 从 AppState 取存储句柄。
fn store_of(app: &AppHandle) -> Result<std::sync::Arc<cc_server::store::Store>, CommandError> {
    // 存储句柄由 bootstrap 持有在 AppState 之外（它同时被控制面与代理面共享），
    // 这里通过 Tauri 的 state 取回。
    let state = app.state::<AppState>();
    // AppState 不直接持有 Store（避免与 bootstrap 的所有权纠缠），
    // 而是由 bootstrap 存入一个独立的 managed state。
    let _ = state;
    app.try_state::<StoreHandle>()
        .map(|handle| handle.0.clone())
        .ok_or_else(|| CommandError::from("存储句柄尚未就绪".to_string()))
}

/// 存储句柄的 Tauri managed state 包装。
pub struct StoreHandle(pub std::sync::Arc<cc_server::store::Store>);

/// 密钥服务的 Tauri managed state 包装。
///
/// 用 Arc 持有：主密钥从钥匙串加载一次即固定，多个 IPC 调用共享同一实例。
pub struct SecretsHandle(pub std::sync::Arc<Secrets>);
