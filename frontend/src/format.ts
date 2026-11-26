const STATUS_TEXT: Record<string, string> = {
  unconfigured: "未配置",
  connecting: "连接中",
  online: "已连接",
  degraded: "连接异常",
  error: "错误",
};

const SOURCE_TEXT: Record<string, string> = {
  app: "客户端",
  core: "core",
};

export function statusText(status: string | undefined): string {
  if (!status) return "未知";
  return STATUS_TEXT[status] ?? status;
}

export function sourceText(source: string | undefined): string {
  if (!source) return "";
  return SOURCE_TEXT[source] ?? source;
}

// 心跳时间以秒为单位，只显示时分秒。
export function clockTime(unixSeconds: number): string {
  if (!unixSeconds) return "—";
  return new Date(unixSeconds * 1000).toLocaleTimeString("zh-CN", { hour12: false });
}

// 凭据过期、配置更新时间这种按天看的场合只显示到分钟。
export function dateTime(unixSeconds: number): string {
  if (!unixSeconds) return "—";
  const d = new Date(unixSeconds * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  const date = d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate());
  return date + " " + pad(d.getHours()) + ":" + pad(d.getMinutes());
}

export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}
