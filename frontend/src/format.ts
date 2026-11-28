import type { NodeRow } from "./api/types";

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

const PATH_TEXT: Record<string, string> = {
  me: "本机",
  direct: "直连",
  relay: "中继",
};

export function pathText(kind: string | undefined): string {
  if (!kind) return "—";
  return PATH_TEXT[kind] ?? kind;
}

// 拓扑未知（core 在跑但节点与路由查询失败）时的一句话。它与「core 没运行、
// 一台节点都没有」是两件事，列表与拓扑都从这里取，不各写一份。
export const TOPOLOGY_UNKNOWN_TEXT = "拓扑暂不可用";
export const TOPOLOGY_UNKNOWN_HINT = "没拿到节点与路由数据，下一次轮询会自动重试";
/** 指标格里的短说明，放不下整句提示。 */
export const TOPOLOGY_UNKNOWN_NOTE = "拓扑未知";

/**
 * 路径列文案。只有完整路径才整条写出来；`pathIncomplete` 时 `path` 只是已经
 * 走到的那一段，末尾补省略号，空路径则表明连第一段都推不出来。空路径且完整
 * 才是真的「直接可达」。
 */
export function routeText(node: NodeRow): string {
  const hops = node.path.map((hop) => hop.hostname || hop.ip);
  if (!node.pathIncomplete) return hops.length ? hops.join(" → ") : "直接可达";
  return hops.length ? hops.join(" → ") + " → …" : "路径不完整";
}

// 延迟以毫秒下发，未知就是 null/非有限数；显示成 0 会误导人。
// 一律保留一位小数：core 给的是浮点测量值，两位小数会露出 6.877000000000001
// 这种尾巴，而毫秒的个位以下对判断链路质量没有意义。
export function latencyNumber(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "—";
  return ms.toFixed(1);
}

export function latencyText(ms: number | null | undefined): string {
  const value = latencyNumber(ms);
  return value === "—" ? value : value + " ms";
}

// 区间只写一次单位：`6.9 – 27.6 ms`；两端相同时只写一个数，不写 `6.9 – 6.9`。
export function latencyRangeText(min: number, max: number): string {
  const low = latencyNumber(min);
  const high = latencyNumber(max);
  if (low === "—" || high === "—") return "—";
  return low === high ? low + " ms" : low + " – " + high + " ms";
}

export type LatencyClass = "fast" | "medium" | "slow" | "unknown";

// 延迟着色的唯一判据：< 50ms 绿、50–100ms 黄、≥ 100ms 红；未知不进任何一档。
// 颜色只表达延迟，路径种类（直连/中继）由「路径」列的标签表达。
export function latencyClass(ms: number | null | undefined): LatencyClass {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "unknown";
  if (ms < 50) return "fast";
  if (ms < 100) return "medium";
  return "slow";
}

// core 的版本串形如 2.6.4-8428a89d，列表只显示发行号。
export function releaseVersion(version: string | undefined): string {
  if (!version) return "—";
  const cut = version.indexOf("-");
  return cut > 0 ? version.slice(0, cut) : version;
}

export function joinOrDash(items: string[] | undefined): string {
  const kept = (items ?? []).filter((item) => item.trim() !== "");
  return kept.length ? kept.join(",") : "—";
}

// ---- 节点表排序 ----

export type SortDirection = "asc" | "desc";

/** 可排序的列；键名只是内部标识，界面文案在 DataView 的表头里。 */
export type NodeSortKey = "name" | "ip" | "route" | "proto" | "nat" | "version" | "latency";

export interface NodeSort {
  key: NodeSortKey;
  direction: SortDirection;
}

/**
 * 表头点击的三态循环：默认 → 正序 → 倒序 → 默认。换一列从正序开始；
 * 回到默认就是「不排序」，调用方原样使用 core 给的顺序，不再套一次排序。
 */
export function nextSort(current: NodeSort | null, key: NodeSortKey): NodeSort | null {
  if (!current || current.key !== key) return { key, direction: "asc" };
  if (current.direction === "asc") return { key, direction: "desc" };
  return null;
}

function compareText(a: string, b: string): number {
  const left = a.toLowerCase();
  const right = b.toLowerCase();
  if (left !== right) return left < right ? -1 : 1;
  if (a === b) return 0;
  return a < b ? -1 : 1;
}

// 节点名自然序：数字段按数值比，node2 排在 node10 前面。同一位置上数字段排在
// 文字段之前，其余按文本比；前缀相同的再按段数补齐。
function compareNatural(a: string, b: string): number {
  const left = a.match(/\d+|\D+/g) ?? [];
  const right = b.match(/\d+|\D+/g) ?? [];
  const shared = Math.min(left.length, right.length);
  for (let i = 0; i < shared; i += 1) {
    const x = left[i] ?? "";
    const y = right[i] ?? "";
    const xNumeric = /^\d+$/.test(x);
    const yNumeric = /^\d+$/.test(y);
    if (xNumeric && yNumeric) {
      const diff = Number(x) - Number(y);
      if (diff !== 0) return diff;
      continue;
    }
    if (xNumeric !== yNumeric) return xNumeric ? -1 : 1;
    const diff = compareText(x, y);
    if (diff !== 0) return diff;
  }
  return left.length - right.length;
}

/** IPv4 的数值形式；不是合法 IPv4 时返回 null，调用方退回文本比较。 */
export function ipv4Key(ip: string): number | null {
  const parts = ip.replace(/\/\d+$/, "").split(".");
  if (parts.length !== 4) return null;
  let key = 0;
  for (const part of parts) {
    if (!/^\d{1,3}$/.test(part)) return null;
    const octet = Number(part);
    if (octet > 255) return null;
    key = key * 256 + octet;
  }
  return key;
}

// 路径列的稳定秩：直达 < 中继 < 未知（本机不在排序范围内，落到未知即可）。
function pathRank(kind: string | undefined): number {
  if (kind === "direct") return 0;
  if (kind === "relay") return 1;
  return 2;
}

// 版本按数值段比：2.10.0 高于 2.9.9，不能按字符串。发行号取不出来时退回文本比。
function versionSegments(version: string | undefined): number[] {
  return (releaseVersion(version).match(/\d+/g) ?? []).map(Number);
}

function compareVersion(a: string | undefined, b: string | undefined): number {
  const left = versionSegments(a);
  const right = versionSegments(b);
  const shared = Math.min(left.length, right.length);
  for (let i = 0; i < shared; i += 1) {
    const diff = (left[i] ?? 0) - (right[i] ?? 0);
    if (diff !== 0) return diff;
  }
  if (left.length !== right.length) return left.length - right.length;
  return compareText(releaseVersion(a), releaseVersion(b));
}

function knownLatency(ms: number | null | undefined): number | null {
  return ms === null || ms === undefined || !Number.isFinite(ms) ? null : ms;
}

function compareByKey(a: NodeRow, b: NodeRow, key: Exclude<NodeSortKey, "latency">): number {
  switch (key) {
    case "name":
      return compareNatural(a.hostname || a.ip, b.hostname || b.ip);
    case "ip": {
      const left = ipv4Key(a.ip);
      const right = ipv4Key(b.ip);
      if (left !== null && right !== null) return left - right;
      return compareText(a.ip, b.ip);
    }
    case "route": {
      const rank = pathRank(a.pathKind) - pathRank(b.pathKind);
      if (rank !== 0) return rank;
      const hops = a.hops - b.hops;
      if (hops !== 0) return hops;
      return compareText(routeText(a), routeText(b));
    }
    case "proto":
      return compareText(joinOrDash(a.tunnels), joinOrDash(b.tunnels));
    case "nat":
      return compareText(a.natType || "—", b.natType || "—");
    case "version":
      return compareVersion(a.version, b.version);
    default:
      return 0;
  }
}

/** 按列比较两行；延迟未知的行永远排最后，正序倒序都一样。 */
export function compareNodes(
  a: NodeRow,
  b: NodeRow,
  key: NodeSortKey,
  direction: SortDirection,
): number {
  if (key === "latency") {
    const left = knownLatency(a.latencyMs);
    const right = knownLatency(b.latencyMs);
    if (left === null || right === null) {
      if (left === right) return 0;
      return left === null ? 1 : -1;
    }
    return direction === "desc" ? right - left : left - right;
  }
  const result = compareByKey(a, b, key);
  return direction === "desc" ? -result : result;
}

/**
 * 排序后的副本。同值行保持 core 给的相对顺序（sort 是稳定排序），
 * 所以每秒轮询换掉行对象也不会让表格无谓地抖动。
 */
export function sortNodes(
  nodes: NodeRow[],
  key: NodeSortKey,
  direction: SortDirection,
): NodeRow[] {
  return [...nodes].sort((a, b) => compareNodes(a, b, key, direction));
}
