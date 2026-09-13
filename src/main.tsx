import React from "react";
import ReactDOM from "react-dom/client";

import { App } from "./App";
import { applyTheme, getInitialThemeMode } from "./lib/theme";
import "./styles.css";

// 尽早注入主题属性，防止页面加载瞬间闪烁
applyTheme(getInitialThemeMode());

const root = document.getElementById("root");
if (root === null) {
  // 入口缺失是打包配置错误，直接抛出比白屏更容易定位
  throw new Error("找不到 #root 挂载点：index.html 与前端入口不匹配");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
