// 与 Go 侧 engine/binding 的 JSON 结构一一对应（见 internal/engine/views.go）。

export type View = "data" | "logs" | "settings";

export type Status = "unconfigured" | "connecting" | "online" | "degraded" | "error";

export type UpdateState = "" | "downloading" | "ready" | "error";

export interface Snapshot {
  status: Status | "";
  serverUrl: string;
  deviceId: string;
  hostname: string;
  hostnameSet: string;
  configHash: string;
  corePath: string;
  corePathUsed: string;
  coreRunning: boolean;
  corePid: number;
  coreVersion: string;
  /** 本代 core 拉起的时间（unix 秒），0 表示还没起来。 */
  coreStartedAt: number;
  localIp: string;
  /** STUN 探测到的公网出口地址，core 还没探完时为空数组。 */
  publicIps: string[];
  /** core 判定的 NAT 类型（Unknown、PortRestricted、Symmetric…）。 */
  natTypeUdp: string;
  natTypeTcp: string;
  networkName: string;
  /** 认证方式："formal" 节点私钥、"credential" 凭据，空表示还没有身份。 */
  authMode: string;
  /** 凭据过期时刻（unix 秒）；正式节点与还没记录的都是 0。 */
  credentialExpiry: number;
  /** config.toml 的修改时间（unix 秒），0 表示没有配置。 */
  configUpdatedAt: number;
  /** 配置里的加密算法原文，空表示配置没写这一项。 */
  encryption: string;
  lastHeartbeat: number;
  lastError: string;
  appVersion: string;
  updateState: UpdateState;
  updateVersion: string;
  updateNotes: string;
  updateError: string;
  /** 当前平台是否支持开机自启；非 Windows 为 false，界面据此隐藏开关。 */
  autostartSupported: boolean;
  autostartEnabled: boolean;
}

/** Hop 是路由路径上的一个中间节点。 */
export interface Hop {
  ip: string;
  hostname: string;
}

export interface NodeRow {
  ip: string;
  hostname: string;
  /** `me` / `direct` / `relay`，界面按它取标签与颜色。 */
  pathKind: string;
  /** 从本机到该节点途经的中间节点，按顺序排列，不含目标自身；直连为空。 */
  path: Hop[];
  hops: number;
  /** 本行用到的隧道协议，已去重排序；本机那一行是整张网的聚合。 */
  tunnels: string[];
  /** `ListRoute` 给的原值，可能是空串。 */
  natType: string;
  /** 到该节点的延迟（毫秒）；null 表示未知。 */
  latencyMs: number | null;
  /** core 上报的完整版本串；截取发行号由前端做。 */
  version: string;
  proxyCidrs: string[];
}

export interface Dashboard {
  snapshot: Snapshot | null;
  nodes: NodeRow[] | null;
}

export interface LogLine {
  time: string;
  source: string;
  text: string;
  /** 后端把相邻的相同行合并后，这里是被合并掉的条数；1 表示只有一条。 */
  count?: number;
}

export interface SettingsInput {
  serverUrl: string;
  token: string;
  hostname: string;
  corePath: string;
}