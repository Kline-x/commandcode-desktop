/**
 * 控制 API 客户端。
 *
 * 与 cc-server 暴露的本地控制端点通信（见 docs/PLAN.md 第 11 节）。
 * 所有请求都带 `x-control-token`：控制端口是随机的，token 每次启动生成，
 * 只有本应用的 WebView 知道——这样本机其他网页无法通过回环地址读账号。
 */

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
  return getJson<Health>("/health");
}
