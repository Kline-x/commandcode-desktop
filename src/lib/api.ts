/**
 * 控制 API 客户端。
 *
 * 与 cc-server 暴露的本地控制端点通信（见 docs/PLAN.md 第 11 节）。
 * 所有请求都带 `x-control-token`：控制端口是随机的，token 每次启动生成，
 * 只有本应用的 WebView 知道——这样本机其他网页无法通过回环地址读账号。
 */

import { invoke } from "@tauri-apps/api/core";

/** 控制 API 基址与 token 由 Tauri 注入到 window。 */
declare global {
  interface Window {
    __CC_CONTROL__?: { baseUrl: string; token: string };
  }
}

/** 从注入的配置里取控制面地址；未注入时（浏览器直接打开）走同源。 */
function controlBase(): string {
  return window.__CC_CONTROL__?.baseUrl ?? "";
}

/** 统一的请求头。 */
function headers(): HeadersInit {
  const token = window.__CC_CONTROL__?.token;
  const base: Record<string, string> = { "content-type": "application/json" };
  if (token !== undefined) {
    base["x-control-token"] = token;
  }
  return base;
}

/** 发一个 GET 并解析 JSON，失败时抛出带状态码的错误。 */
async function getJson<T>(path: string): Promise<T> {
  const response = await fetch(`${controlBase()}${path}`, { headers: headers() });
  if (!response.ok) {
    throw new Error(`${path} 返回 ${response.status}`);
  }
  return (await response.json()) as T;
}

/** 发一个带 JSON body 的请求。 */
export async function sendJson<T>(
  method: "POST" | "PATCH" | "PUT" | "DELETE",
  path: string,
  body?: unknown,
): Promise<T> {
  const response = await fetch(`${controlBase()}${path}`, {
    method,
    headers: headers(),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) {
    // 控制面的错误信封是 { error: string }，尽量把原因透出来
    const detail = await response.text().catch(() => "");
    throw new Error(`${method} ${path} 返回 ${response.status}${detail ? `: ${detail}` : ""}`);
  }
  return (await response.json()) as T;
}

/** 代理健康状态。 */
export interface Health {
  status: string;
  accounts: number;
  api_base: string;
}

/** 查询代理健康状态。 */
export function getHealth(): Promise<Health> {
  return getJson<Health>("/api/health");
}

/** 新增账号的返回。 */
export interface AddAccountResult {
  id: number;
  key_hint: string;
}

/**
 * 新增一个账号。
 *
 * **走 Tauri IPC 而不是控制面 HTTP**：明文密钥的加密必须发生在 Rust 侧。
 * 若让前端自己调 `/api/accounts`，它就得自己加密——等于把加密逻辑与主密钥
 * 暴露给 WebView。这里把明文交给 Rust，加密与落库都在那边完成，
 * 返回值里只有提示（形如 `user_…ab12`）。
 */
export function addAccount(label: string, apiKey: string): Promise<AddAccountResult> {
  return invoke<AddAccountResult>("add_account", { label, apiKey });
}

/** 打开应用本地数据目录。 */
export function revealDataDir(): Promise<void> {
  return invoke<void>("reveal_data_dir");
}

/** 路由规则对象。 */
export interface RouteRule {
  id: number;
  models: string[];
  account_id: string;
}

/** 获取全部路由规则。 */
export async function getRules(): Promise<RouteRule[]> {
  const data = await getJson<{ rules: RouteRule[] }>("/api/rules");
  return data.rules;
}

/** 替换保存全部路由规则。 */
export async function updateRules(rules: { models: string[]; account_id: string }[]): Promise<void> {
  await sendJson<{ ok: boolean }>("PUT", "/api/rules", { rules });
}

/** 全局设置结构。 */
export interface Settings {
  retention: number;
  api_base: string;
}

/** 获取全局设置。 */
export function getSettings(): Promise<Settings> {
  return getJson<Settings>("/api/settings");
}

/** 修改全局设置项。 */
export async function setSetting(key: string, value: string): Promise<void> {
  await sendJson<{ ok: boolean }>("PUT", `/api/settings/${encodeURIComponent(key)}`, { value });
}

/** 运行日志条目。 */
export interface LogEntry {
  id: number;
  timestamp_ms: number;
  level: "INFO" | "WARN" | "ERROR" | "DEBUG" | string;
  message: string;
}

/** 获取最近运行日志。 */
export async function getLogs(limit = 200): Promise<LogEntry[]> {
  const data = await getJson<{ logs: LogEntry[] }>(`/api/logs?limit=${limit}`);
  return data.logs;
}

/** 清空运行日志。 */
export async function clearLogs(): Promise<void> {
  await sendJson<{ ok: boolean }>("POST", "/api/logs/clear");
}

/** 触发全量账号配额即时刷新。 */
export async function refreshAccounts(): Promise<void> {
  await sendJson<{ ok: boolean }>("POST", "/api/accounts/refresh");
}

/** 更新指定账号设置（改名 / 启停）。 */
export async function updateAccount(
  id: number,
  fields: { label?: string; enabled?: boolean },
): Promise<void> {
  await sendJson<{ ok: boolean }>("PATCH", `/api/accounts/${id}`, fields);
}

