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
      // src-tauri 由 cargo 自己 watch，Vite 不必重复扫描
      ignored: ["**/src-tauri/**", "**/crates/**"],
    },
  },
  build: {
    // Tauri 使用的 WebView 版本可控，无需兼容过老的浏览器
    target: "es2022",
    sourcemap: false,
  },
});
