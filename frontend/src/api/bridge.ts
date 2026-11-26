import { errorMessage } from "../format";
import type { Dashboard, LogLine, SettingsInput, Snapshot } from "./types";

// 连接断了与“服务端回了个错误”是两件事：前者（客户端正在退出、正在装新版本）等着它
// 回来就行，后者要把原因显示给人看。调用方要能分开，所以单独一个类型。
export class OfflineError extends Error {}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const init: RequestInit = { method, headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  let res: Response;
  try {
    res = await fetch(path, init);
  } catch (err) {
    throw new OfflineError(errorMessage(err));
  }
  if (!res.ok) {
    let message = "HTTP " + res.status;
    try {
      const data = (await res.json()) as { error?: string };
      if (data?.error) message = data.error;
    } catch {
      // 非 JSON 响应就用状态码。
    }
    throw new Error(message);
  }
  if (res.status === 204) return null as T;
  return (await res.json()) as T;
}

export const bridge = {
  snapshot: () => call<Snapshot>("GET", "/api/snapshot"),
  dashboard: () => call<Dashboard>("GET", "/api/dashboard"),
  logs: (lines?: number) => call<LogLine[]>("GET", "/api/logs" + (lines ? "?lines=" + lines : "")),
  events: (lines?: number) => call<LogLine[]>("GET", "/api/events" + (lines ? "?lines=" + lines : "")),
  reconnect: () => call<null>("POST", "/api/reconnect"),
  saveSettings: (input: SettingsInput) => call<Snapshot>("PUT", "/api/settings", input),
  setAutostart: (enabled: boolean) => call<Snapshot>("PUT", "/api/autostart", { enabled }),
  resetConfig: () => call<Snapshot>("POST", "/api/reset"),
  applyUpdate: () => call<null>("POST", "/api/update"),
};
