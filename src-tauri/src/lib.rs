//! Tauri 外壳的库入口。
//!
//! 拆成 lib + bin 是 Tauri 2 的惯例：移动端需要库目标，桌面端只需一个 bin。

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

mod bootstrap;
mod commands;
mod secrets;
mod tray;

/// 应用状态：后台服务的句柄与前端需要的连接信息。
pub struct AppState {
    /// 控制面地址（含端口）。
    pub control_base_url: String,
    /// 控制面 token。
    pub control_token: String,
    /// 代理面地址。
    pub proxy_base_url: String,
    /// 数据库路径（用于「打开数据目录」菜单）。
    pub data_dir: std::path::PathBuf,
    /// 配额轮询的停机信号发送端（进程退出时置真）。
    pub quota_shutdown: tokio::sync::watch::Sender<bool>,
}

/// 启动应用。
pub fn run() {
    // 日志：默认 info，可用 RUST_LOG 覆盖。写到 stderr，由宿主决定是否重定向。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 单实例：第二次启动时把已有窗口带到前台，而不是起第二个代理
    //（两个代理抢同一个端口会有一个静默失败，用户看到的却是「没反应」）。
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
                let _ = window.unminimize();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let handle = app.handle().clone();
            let (state, store, secrets, reloader) = match bootstrap::start(&handle) {
                Ok(tuple) => tuple,
                Err(error) => {
                    // 启动失败必须让用户看见原因，而不是留一个空窗口
                    tracing::error!(%error, "后台服务启动失败");
                    return Err(Box::new(error));
                }
            };

            // 把控制面地址与 token 作为 initialization script 注入：
            // 它在页面任何 JS 之前执行，因此 React 的首个 fetch 就能带上 token
            // （若改用 build 之后的 eval，首屏请求会先发出并拿到 401）。
            let init_script = bootstrap::init_script(&state.control_base_url, &state.control_token);
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Command Code")
                .inner_size(1100.0, 760.0)
                .min_inner_size(820.0, 560.0)
                .initialization_script(&init_script)
                .build()?;

            tray::install(&handle, &state)?;
            // 存储、密钥句柄与重载器单独 manage：IPC 命令需要它们
            app.manage(commands::StoreHandle(store));
            app.manage(commands::SecretsHandle(secrets));
            app.manage(reloader);
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::add_account,
            commands::control_endpoint,
            commands::reveal_data_dir,
        ])
        .on_window_event(|window, event| {
            // 关闭窗口 = 隐藏到托盘；真正退出走托盘菜单。
            // 这与「关窗不停止代理」的产品定义一致（docs/PLAN.md 第 1 节）。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        });

    builder
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
