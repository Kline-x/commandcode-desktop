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
    // 优先复用配置中声明的托盘实例；若未声明则动态构建
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu))?;
        tray.set_show_menu_on_left_click(true)?;
        tray.on_menu_event(move |app, event| match event.id().as_ref() {
            SHOW => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.unminimize();
                }
            }
            COPY_ENDPOINT => {
                #[cfg(target_os = "macos")]
                {
                    use std::io::Write;
                    if let Ok(mut child) = std::process::Command::new("pbcopy")
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                    {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(endpoint.as_bytes());
                        }
                    }
                }
                tracing::info!(endpoint = %endpoint, "本地端点已复制");
            }
            QUIT => app.exit(0),
            _ => {}
        });
        tray.on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.unminimize();
                }
            }
        });
        return Ok(());
    }

    let icon_bytes = include_bytes!("../icons/32x32.png");
    let icon = tauri::image::Image::from_bytes(icon_bytes)?;

    let tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(false)
        .show_menu_on_left_click(true)
        .menu(&menu)
        .tooltip("Command Code")
        .on_menu_event(move |app, event| match event.id().as_ref() {
            SHOW => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.unminimize();
                }
            }
            COPY_ENDPOINT => {
                #[cfg(target_os = "macos")]
                {
                    use std::io::Write;
                    if let Ok(mut child) = std::process::Command::new("pbcopy")
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                    {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(endpoint.as_bytes());
                        }
                    }
                }
                tracing::info!(endpoint = %endpoint, "本地端点已复制");
            }
            QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.unminimize();
                }
            }
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
