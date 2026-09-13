import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getSettings, revealDataDir, setSetting } from "../lib/api";

export function SettingsPanel(): React.JSX.Element {
  const [retention, setRetention] = useState("5000");
  const [apiBase, setApiBase] = useState("https://api.commandcode.ai");
  const [autoStart, setAutoStart] = useState<boolean>(false);
  const [autoStartAvailable, setAutoStartAvailable] = useState<boolean>(true);
  const [saving, setSaving] = useState<boolean>(false);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  useEffect(() => {
    // 载入设置项
    void getSettings()
      .then((s) => {
        setRetention(String(s.retention || 5000));
        if (s.api_base) setApiBase(s.api_base);
      })
      .catch((e) => {
        setMsg({ type: "err", text: `加载配置失败: ${String(e)}` });
      });

    // 检查自启动状态
    invoke<boolean>("plugin:autostart|is_enabled")
      .then((enabled) => setAutoStart(enabled))
      .catch(() => setAutoStartAvailable(false));
  }, []);

  const handleSaveRetention = async () => {
    const val = parseInt(retention, 10);
    if (isNaN(val) || val <= 0) {
      setMsg({ type: "err", text: "流水保留条数必须是正整数" });
      return;
    }
    try {
      setSaving(true);
      setMsg(null);
      await setSetting("retention", String(val));
      setMsg({ type: "ok", text: "设置已保存成功" });
      setTimeout(() => setMsg(null), 3000);
    } catch (e) {
      setMsg({ type: "err", text: `保存失败: ${String(e)}` });
    } finally {
      setSaving(false);
    }
  };

  const handleToggleAutoStart = async () => {
    try {
      if (autoStart) {
        await invoke("plugin:autostart|disable");
        setAutoStart(false);
      } else {
        await invoke("plugin:autostart|enable");
        setAutoStart(true);
      }
      setMsg({ type: "ok", text: `开机自启已${!autoStart ? "开启" : "关闭"}` });
      setTimeout(() => setMsg(null), 3000);
    } catch (e) {
      setMsg({ type: "err", text: `自启动设置失败: ${String(e)}` });
    }
  };

  const handleOpenDataDir = async () => {
    try {
      await revealDataDir();
    } catch (e) {
      setMsg({ type: "err", text: `打开目录失败: ${String(e)}` });
    }
  };

  const copyToClipboard = async (text: string, key: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedKey(key);
      setTimeout(() => setCopiedKey(null), 2000);
    } catch {
      // 剪贴板不可用时静默
    }
  };

  return (
    <section className="panel settings-panel">
      <div className="panel__header">
        <div>
          <h2>全局设置</h2>
          <p className="panel__desc">查看与调整代理服务连接参数、系统行为及数据存储策略。</p>
        </div>
      </div>

      {msg && <div className={`notice notice--${msg.type}`}>{msg.text}</div>}

      <div className="settings-sections">
        {/* 端点配置说明 */}
        <div className="settings-group">
          <h3>代理服务接入端点</h3>
          <p className="settings-tip">
            本应用在本地启动代理端口。将您的开发工具（如 Cursor、VS Code、Claude Code、Aider 等）指向以下本地地址：
          </p>

          <div className="endpoint-list">
            <div className="endpoint-item">
              <span className="endpoint-label">OpenAI Base URL:</span>
              <code>http://127.0.0.1:3050/v1</code>
              <button
                className="btn btn--sm"
                onClick={() => void copyToClipboard("http://127.0.0.1:3050/v1", "openai")}
              >
                {copiedKey === "openai" ? "已复制 ✓" : "复制"}
              </button>
            </div>

            <div className="endpoint-item">
              <span className="endpoint-label">Anthropic Base URL:</span>
              <code>http://127.0.0.1:3050</code>
              <button
                className="btn btn--sm"
                onClick={() => void copyToClipboard("http://127.0.0.1:3050", "anthropic")}
              >
                {copiedKey === "anthropic" ? "已复制 ✓" : "复制"}
              </button>
            </div>

            <div className="endpoint-item">
              <span className="endpoint-label">上游 API 基址:</span>
              <code>{apiBase}</code>
            </div>
          </div>
        </div>

        {/* 系统与数据策略 */}
        <div className="settings-group">
          <h3>系统与存储</h3>

          {autoStartAvailable && (
            <div className="setting-row">
              <div className="setting-info">
                <strong>开机自动启动</strong>
                <p>登录 macOS 系统时自动在后台托盘拉起代理服务</p>
              </div>
              <label className="switch">
                <input
                  type="checkbox"
                  checked={autoStart}
                  onChange={() => void handleToggleAutoStart()}
                />
                <span className="slider round"></span>
              </label>
            </div>
          )}

          <div className="setting-row">
            <div className="setting-info">
              <strong>请求流水保留上限</strong>
              <p>控制 SQLite 数据库中保留的最大历史请求流水条数（默认 5000 条，超出自动截断清理）</p>
            </div>
            <div className="setting-action">
              <input
                type="number"
                min="100"
                max="50000"
                step="500"
                style={{ width: "100px" }}
                value={retention}
                onChange={(e) => setRetention(e.target.value)}
              />
              <button
                className="btn btn--secondary btn--sm"
                onClick={() => void handleSaveRetention()}
                disabled={saving}
              >
                {saving ? "保存中" : "保存"}
              </button>
            </div>
          </div>

          <div className="setting-row">
            <div className="setting-info">
              <strong>应用数据目录</strong>
              <p>包含 SQLite 数据库文件 (commandcode.db)、主密钥文件 (.master_key) 及崩溃日志</p>
            </div>
            <button className="btn btn--secondary btn--sm" onClick={() => void handleOpenDataDir()}>
              在 Finder 中打开
            </button>
          </div>
        </div>
      </div>
    </section>
  );
}
