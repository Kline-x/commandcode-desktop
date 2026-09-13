import { useCallback, useEffect, useState } from "react";

import { AccountsPanel } from "./components/AccountsPanel";
import { LogsPanel } from "./components/LogsPanel";
import { ProxyPanel } from "./components/ProxyPanel";
import { RequestsPanel } from "./components/RequestsPanel";
import { RulesPanel } from "./components/RulesPanel";
import { SettingsPanel } from "./components/SettingsPanel";
import { getHealth, type Health } from "./lib/api";

type TabKey = "dashboard" | "requests" | "rules" | "settings" | "logs";

/**
 * 应用主容器与工作区导航。
 */
export function App(): React.JSX.Element {
  const [activeTab, setActiveTab] = useState<TabKey>("dashboard");
  const [health, setHealth] = useState<Health | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async (): Promise<void> => {
    try {
      setHealth(await getHealth());
      setError(null);
    } catch (cause) {
      setHealth(null);
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 5_000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  return (
    <div className="app">
      <header className="app__header">
        <div className="app__branding">
          <h1>Command Code</h1>
          <span className={`badge ${health ? "badge--ok" : "badge--down"}`}>
            {health ? "代理运行中 (3050)" : "代理未就绪"}
          </span>
        </div>

        <nav className="nav-tabs">
          <button
            className={`nav-tab ${activeTab === "dashboard" ? "nav-tab--active" : ""}`}
            onClick={() => setActiveTab("dashboard")}
          >
            📊 仪表盘
          </button>
          <button
            className={`nav-tab ${activeTab === "requests" ? "nav-tab--active" : ""}`}
            onClick={() => setActiveTab("requests")}
          >
            ⚡ 请求流水
          </button>
          <button
            className={`nav-tab ${activeTab === "rules" ? "nav-tab--active" : ""}`}
            onClick={() => setActiveTab("rules")}
          >
            🔀 路由规则
          </button>
          <button
            className={`nav-tab ${activeTab === "settings" ? "nav-tab--active" : ""}`}
            onClick={() => setActiveTab("settings")}
          >
            ⚙️ 全局设置
          </button>
          <button
            className={`nav-tab ${activeTab === "logs" ? "nav-tab--active" : ""}`}
            onClick={() => setActiveTab("logs")}
          >
            📝 运行日志
          </button>
        </nav>
      </header>

      {error !== null && (
        <p className="notice notice--warn">
          无法连接本地服务：{error}
          <br />
          请确认后台进程已就绪；连接恢复后界面会自动同步。
        </p>
      )}

      <main className="app__main">
        {activeTab === "dashboard" && (
          <>
            <ProxyPanel health={health} />
            <AccountsPanel />
          </>
        )}
        {activeTab === "requests" && <RequestsPanel />}
        {activeTab === "rules" && <RulesPanel />}
        {activeTab === "settings" && <SettingsPanel />}
        {activeTab === "logs" && <LogsPanel />}
      </main>
    </div>
  );
}
