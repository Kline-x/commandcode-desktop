import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 期望一个固定端口的开发服务器，且失败时不要静默换端口
// （否则窗口会去加载一个不存在的地址，表现为白屏）
export default defineConfig({
  plugins: [react()],
  // 用相对路径：Tauri 在生产构建下通过自定义协议加载资源，
  // 绝对路径（/assets/...）在部分平台会解析失败，表现为白屏。
  base: "./",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      // src-tauri / crates 由 cargo 自己 watch，Vite 不必重复扫描。
      //
      // ⚠️ `target/` 必须一并忽略：cargo 构建时会写入并**锁定** target 下的
      // 临时 DLL（proc-macro 产物，如 cssparser_macros-*.dll）。chokidar 一旦
      // 尝试 watch 这些被独占的文件就会抛 EBUSY 并**直接杀掉 dev server**，
      // 表现为「跑着跑着 Vite 退出，窗口变成 localhost 拒绝连接」。
      // target/ 是纯构建产物，前端没有理由监听它。
      ignored: ["**/target/**", "**/src-tauri/**", "**/crates/**"],
    },
  },
  build: {
    // Tauri 使用的 WebView 版本可控，无需兼容过老的浏览器
    target: "es2022",
    sourcemap: false,
  },
});
