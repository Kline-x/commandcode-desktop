//! Command Code 桌面客户端入口。
//!
//! 本 crate 是**薄外壳**：窗口、托盘、单实例、密钥与后台服务的编排。
//! 所有协议与业务逻辑都在 `cc-server`（平台无关，可在 Linux CI 独立测试）。
//!
//! 启动顺序很重要：
//! 1. 打开数据库（失败则退出——没有存储就无法工作）；
//! 2. 生成随机控制 token，起控制面与代理面（各自独立端口）；
//! 3. 把控制面地址与 token 注入 WebView；
//! 4. 建窗口与托盘。
//!
//! 控制面与代理面**分端口**：代理面要能被任意本地客户端直接调用，而控制面
//! 能读账号列表，必须用 token 保护（docs/ARCHITECTURE.md 第 7 节）。

// Windows 下不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    commandcode_desktop_lib::run()
}
