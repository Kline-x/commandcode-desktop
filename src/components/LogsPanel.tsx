import { useCallback, useEffect, useRef, useState } from "react";
import { clearLogs, getLogs, type LogEntry } from "../lib/api";

export function LogsPanel(): React.JSX.Element {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [filterLevel, setFilterLevel] = useState<string>("ALL");
  const [search, setSearch] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const scrollRef = useRef<HTMLDivElement | null>(null);

  const fetchLogs = useCallback(async () => {
    try {
      const items = await getLogs(500);
      setLogs(items);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void fetchLogs();
    const interval = window.setInterval(() => void fetchLogs(), 2500);
    return () => window.clearInterval(interval);
  }, [fetchLogs]);

  useEffect(() => {
    if (autoScroll && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [logs, autoScroll]);

  const handleClear = async () => {
    try {
      await clearLogs();
      setLogs([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const handleExport = () => {
    const text = logs
      .map((l) => {
        const time = new Date(l.timestamp_ms).toISOString();
        return `[${time}] [${l.level}] ${l.message}`;
      })
      .join("\n");
    const blob = new Blob([text], { type: "text/plain;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `commandcode-desktop-${new Date().toISOString().slice(0, 10)}.log`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const filteredLogs = logs.filter((l) => {
    if (filterLevel !== "ALL" && l.level !== filterLevel) {
      return false;
    }
    if (search.trim() && !l.message.toLowerCase().includes(search.toLowerCase())) {
      return false;
    }
    return true;
  });

  return (
    <section className="panel logs-panel">
      <div className="panel__header">
        <div>
          <h2>运行日志与诊断</h2>
          <p className="panel__desc">
            后台服务内存循环缓冲区（Ring Buffer，最新 500 条）。排查请求失败、通道切换与鉴权异常。
          </p>
        </div>
        <div className="logs-actions">
          <button className="btn btn--secondary btn--sm" onClick={() => void fetchLogs()}>
            刷新
          </button>
          <button className="btn btn--secondary btn--sm" onClick={handleExport} disabled={logs.length === 0}>
            导出日志
          </button>
          <button className="btn btn--danger btn--sm" onClick={() => void handleClear()} disabled={logs.length === 0}>
            清空
          </button>
        </div>
      </div>

      {error && <div className="notice notice--err">{error}</div>}

      {/* 过滤条 */}
      <div className="logs-filter-bar">
        <div className="filter-group">
          <label>级别：</label>
          <select value={filterLevel} onChange={(e) => setFilterLevel(e.target.value)}>
            <option value="ALL">全部 (ALL)</option>
            <option value="INFO">信息 (INFO)</option>
            <option value="WARN">警告 (WARN)</option>
            <option value="ERROR">错误 (ERROR)</option>
            <option value="DEBUG">调试 (DEBUG)</option>
          </select>
        </div>

        <div className="filter-group" style={{ flex: 1 }}>
          <input
            type="text"
            placeholder="搜索日志关键词…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </div>

        <div className="filter-group">
          <label className="checkbox-label">
            <input
              type="checkbox"
              checked={autoScroll}
              onChange={(e) => setAutoScroll(e.target.checked)}
            />
            自动滚屏
          </label>
        </div>
      </div>

      {/* 日志终端显示区 */}
      <div className="logs-console" ref={scrollRef}>
        {loading && logs.length === 0 ? (
          <div className="logs-console__empty">正在获取最新日志…</div>
        ) : filteredLogs.length === 0 ? (
          <div className="logs-console__empty">没有匹配的日志记录。</div>
        ) : (
          filteredLogs.map((log) => {
            const timeStr = new Date(log.timestamp_ms).toLocaleTimeString();
            const levelClass =
              log.level === "ERROR"
                ? "log--err"
                : log.level === "WARN"
                ? "log--warn"
                : log.level === "DEBUG"
                ? "log--debug"
                : "log--info";
            return (
              <div key={log.id} className={`log-line ${levelClass}`}>
                <span className="log-time">{timeStr}</span>
                <span className={`log-level ${levelClass}`}>{log.level.padEnd(5)}</span>
                <span className="log-msg">{log.message}</span>
              </div>
            );
          })
        )}
      </div>
    </section>
  );
}
