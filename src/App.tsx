import { useCallback, useEffect, useState } from "react";

import { AccountsPanel } from "./components/AccountsPanel";
import { ProxyPanel } from "./components/ProxyPanel";
import { RequestsPanel } from "./components/RequestsPanel";
import { getHealth, type Health } from "./lib/api";

/**
 * 应用骨架。
 *
 * 三个面板对应三件事：代理状态、账号与额度、请求流水。
 * 状态查询失败不阻塞渲染——首次启动时后端可能还没起来，界面要能自己说明这一点。
 */
export function App(): React.JSX.Element {
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
    // 后端是本地进程，轮询成本极低；首次启动时它可能晚于窗口就绪
    const timer = window.setInterval(() => void refresh(), 5_000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  return (
    <div className="app">
      <header className="app__header">
        <h1>Command Code</h1>
        <span className={`badge ${health ? "badge--ok" : "badge--down"}`}>
          {health ? "代理运行中" : "代理未运行"}
        </span>
      </header>

      {error !== null && (
        <p className="notice notice--warn">
          无法连接本地服务：{error}
          <br />
          请确认应用的后台进程已启动；启动后本页会自动恢复。
        </p>
      )}

      <main className="app__main">
        <ProxyPanel health={health} />
        <AccountsPanel />
        <RequestsPanel />
      </main>
    </div>
  );
}
