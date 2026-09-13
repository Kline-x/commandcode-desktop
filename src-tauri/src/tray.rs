//! 系统托盘：让代理在窗口关闭后继续运行。
//!
//! Linux 的托盘依赖 `libayatana-appindicator`，部分桌面环境（GNOME/Wayland）
//! 默认没有。因此托盘是**增强而非必需**：安装失败时只记日志，应用照常可用
//! （窗口关闭仍会隐藏，用户可从菜单栏/快捷键唤回）。
//! 见 docs/PLAN.md 第 12 节的跨平台注意事项。

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

use crate::AppState;

/// 托盘菜单项 id。
const SHOW: &str = "show";
const COPY_ENDPOINT: &str = "copy-endpoint";
const QUIT: &str = "quit";

/// 安装托盘。失败不是致命错误。
pub fn install(app: &AppHandle, state: &AppState) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, SHOW, "显示主窗口", true, None::<&str>)?;
    let copy = MenuItem::with_id(app, COPY_ENDPOINT, "复制本地端点", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "退出", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(app, &[&show, &copy, &separator, &quit])?;

    let endpoint = format!("{}/v1", state.proxy_base_url);
    let tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("Command Code")
        .on_menu_event(move |app, event| match event.id().as_ref() {
            SHOW => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            COPY_ENDPOINT => {
                // 剪贴板写入交给前端或后续插件；这里先把端点记进日志，
                // 保证菜单项在任何平台上都不会静默无效。
                tracing::info!(endpoint = %endpoint, "本地端点");
            }
            QUIT => app.exit(0),
            _ => {}
        })
        .build(app);

    match tray {
        Ok(_) => Ok(()),
        Err(error) => {
            // 托盘不可用（常见于 Linux 缺 libayatana-appindicator）不该阻止应用运行
            tracing::warn!(%error, "托盘安装失败：应用仍可用，但关闭窗口后需从任务栏唤回");
            Ok(())
        }
    }
}
