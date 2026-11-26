import { computed, ref, shallowRef } from "vue";
import { bridge, OfflineError } from "./api/bridge";
import { errorMessage } from "./format";
import type { LogLine, NodeRow, SettingsInput, Snapshot, Status, View } from "./api/types";

// 全局只有这一份状态：三个页面共用同一个快照，轮询也只有一处。
const view = ref<View>("data");
const snapshot = shallowRef<Snapshot | null>(null);
const nodes = shallowRef<NodeRow[]>([]);
const logs = shallowRef<LogLine[]>([]);
// 两种错误分开：轮询失败每次刷新都会重写，一次性操作（比如点"立即更新"）
// 的结果不应该被下一次成功轮询抹掉。
const pollError = ref("");
const actionError = ref("");
// 安装新版本时进程会退出、面板随之断开，这段时间不该报“后端调用失败”。
const restarting = ref(false);
const banner = computed(() =>
  restarting.value ? "" : actionError.value || pollError.value,
);

export function setActionError(message: string): void {
  actionError.value = message;
}

export function clearActionError(): void {
  actionError.value = "";
} 

// 没有设备身份时自动跳到设置页；用户手动切过就不再打扰。
let autoSwitch = true;
let busy = false;
// 引擎在 goroutine 里读本地配置，启动后的头一两帧 deviceId 必然为空，
// 连续两次都空才认为是真的没配置，否则每次启动都会闪到设置页。
let emptyStreak = 0;

const status = computed<Status | "">(() => snapshot.value?.status ?? "");

export async function refresh(): Promise<void> {
  if (busy || restarting.value) return;
  busy = true;
  try {
    if (view.value === "data") {
      // 仪表盘右侧要挂一串最近事件，顺手一起取，省一次单独的往返。
      // 最近事件由后端筛好（客户端事件 + 内核告警），只回十几行，别拉整页日志。
      const [dashboard, events] = await Promise.all([bridge.dashboard(), bridge.events(12)]);
      snapshot.value = dashboard.snapshot ?? null;
      nodes.value = dashboard.nodes ?? [];
      logs.value = events ?? [];
    } else if (view.value === "logs") {
      const [next, lines] = await Promise.all([bridge.snapshot(), bridge.logs()]);
      snapshot.value = next;
      logs.value = lines ?? [];
    } else {
      snapshot.value = await bridge.snapshot();
    }
    pollError.value = "";
    maybeSwitchToSettings();
  } catch (err) {
    pollError.value = "后端调用失败: " + errorMessage(err);
  } finally {
    busy = false;
  }
}

function maybeSwitchToSettings(): void {
  const current = snapshot.value;
  if (!current) return;
  if (current.deviceId) {
    emptyStreak = 0;
    autoSwitch = true;
    return;
  }
  emptyStreak += 1;
  if (!autoSwitch || view.value === "settings") return;
  // 身份被服务端清掉时引擎会把原因写进 lastError，这种情况要立刻回设置页；
  // 全新安装则连 serverUrl 都没有，等两帧确认不是启动瞬态再跳。
  if (current.lastError) {
    view.value = "settings";
    return;
  }
  if (emptyStreak >= 2 && !current.serverUrl) view.value = "settings";
}

export function select(next: View): void {
  if (next === view.value) return;
  autoSwitch = false;
  clearActionError();
  view.value = next;
  void refresh();
}

export async function saveSettings(input: SettingsInput): Promise<Snapshot> {
  const next = await bridge.saveSettings(input);
  snapshot.value = next;
  autoSwitch = true;
  return next;
}

// 开机自启是即时生效的系统设置，不跟设置页的表单一起提交。
export async function setAutostart(enabled: boolean): Promise<Snapshot> {
  const next = await bridge.setAutostart(enabled);
  snapshot.value = next;
  return next;
}

export async function resetConfig(): Promise<Snapshot> {
  const next = await bridge.resetConfig();
  snapshot.value = next;
  autoSwitch = true;
  return next;
}

// 安装新版本有两条路径：用户在面板点“立即更新”，和下载完自动交接。前者在请求返回
// 之前进程就退出了，所以请求断掉不是失败，而是“正在装”——等新版本把面板拉起来。
export async function applyUpdate(): Promise<void> {
  try {
    await bridge.applyUpdate();
  } catch (err) {
    if (!(err instanceof OfflineError)) throw err;
    await waitForRestart();
  }
}

// 进程退出 → 启动器装包 → 新版本起面板，通常几秒；给到 60 秒。
const RESTART_POLL_MS = 1000;
const RESTART_POLL_LIMIT = 60;

async function waitForRestart(): Promise<void> {
  restarting.value = true;
  actionError.value = "";
  pollError.value = "";
  for (let attempt = 0; attempt < RESTART_POLL_LIMIT; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, RESTART_POLL_MS));
    try {
      await bridge.snapshot();
    } catch {
      // 新版本还没起来，继续等。
      continue;
    }
    location.reload();
    return;
  }
  restarting.value = false;
  setActionError("更新后客户端没有恢复，请从托盘或开始菜单重新打开客户端。");
}

export async function reconnect(): Promise<void> {
  await bridge.reconnect();
  await refresh();
}

export function useClient() {
  return { view, snapshot, nodes, logs, banner, status, restarting, select, refresh, saveSettings, setAutostart, resetConfig, applyUpdate, reconnect, setActionError };
}
