<script setup lang="ts">
import { computed, ref } from "vue";
import { applyUpdate, reconnect, refresh, select, setActionError, useClient } from "../client";
import {
  clockTime,
  dateTime,
  errorMessage,
  joinOrDash,
  latencyClass,
  latencyNumber,
  latencyRangeText,
  latencyText,
  nextSort,
  pathText,
  releaseVersion,
  sortNodes,
  statusText,
  TOPOLOGY_UNKNOWN_HINT,
  TOPOLOGY_UNKNOWN_NOTE,
  TOPOLOGY_UNKNOWN_TEXT,
} from "../format";
import type { NodeRow } from "../api/types";
import type { NodeSort, NodeSortKey } from "../format";
import TopologyView from "./TopologyView.vue";

const { snapshot, nodes, logs, status, restarting } = useClient();

// 节点区域的两种呈现：列表与拓扑图。二者用同一份 nodes 数据，只是画法不同。
const nodeTab = ref<"list" | "topology">("list");

// 节点表有三种事实：core 未运行（空数组）、查询失败（null，拓扑未知）、有行。
// 未知时表格与拓扑图都不渲染，指标也写「—」——不能把「查不出来」说成 0 台。
const known = computed(() => nodes.value !== null);
const nodeCount = computed(() => (nodes.value === null ? null : nodes.value.length));

// 本机在节点表里单独占一行，指标和列表只统计其余节点。
// 这两个派生值只在 known 为真时被渲染，未知时的空数组合并不进画面。
const remote = computed(() => (nodes.value ?? []).filter((node) => node.pathKind !== "me"));
const localRow = computed(() => (nodes.value ?? []).find((node) => node.pathKind === "me") ?? null);
const directCount = computed(() => remote.value.filter((node) => node.pathKind === "direct").length);
const relayCount = computed(() => remote.value.filter((node) => node.pathKind === "relay").length);

// 延迟以毫秒下发，未知是 null；统计只算有数的那些。
const latency = computed(() => {
  const values = remote.value
    .map((node) => node.latencyMs)
    .filter((value): value is number => typeof value === "number" && Number.isFinite(value));
  if (!values.length) return null;
  const sum = values.reduce((total, value) => total + value, 0);
  // 平均值不在这里取整：显示端的「一位小数」由 latencyNumber 统一负责，
  // 中间先取整会把 6.877 变成 7，区间和均值就对不上了。
  return {
    min: Math.min(...values),
    max: Math.max(...values),
    avg: sum / values.length,
  };
});

// core 本代的运行时长；轮询每次都会换掉 snapshot 引用，所以它会跟着走。
const uptime = computed(() => {
  const started = snapshot.value?.coreStartedAt ?? 0;
  if (!started) return "—";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - started);
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (hours > 0) return hours + "h " + minutes + "m";
  if (minutes > 0) return minutes + "m";
  return seconds + "s";
});

const serverHost = computed(() => (snapshot.value?.serverUrl ?? "").replace(/^https?:\/\//, ""));

// 「本机身份」：认证方式只有两种来源，正式节点用节点私钥，其余用控制面签发的凭据。
const AUTH_TEXT: Record<string, string> = { formal: "节点私钥", credential: "凭据" };

const authMode = computed(() => snapshot.value?.authMode ?? "");
const authText = computed(() => AUTH_TEXT[authMode.value] ?? "—");
const authHint = computed(() => {
  if (authMode.value === "formal") return "正式节点：配置里有 network_secret，身份是 secure_mode 的节点私钥";
  if (authMode.value === "credential") return "凭据节点：身份是控制面签发的凭据（config/credential.json）";
  return "还没有身份：未注册，或配置与凭据都还没下发";
});

// 正式节点没有凭据，这一行整行不显示；凭据节点还没记下过期时刻时如实写"未知"。
const credential = computed(() => {
  if (authMode.value !== "credential") return null;
  const at = snapshot.value?.credentialExpiry ?? 0;
  if (!at) {
    return { text: "未知", warn: false, hint: "旧版本的凭据文件里没有记这一项，控制面下次签发时补上" };
  }
  // 服务端给的是绝对到期时刻，剩余天数在这里现算；侧栏窄，先写天数再写时刻，
  // 被截断时留下的是要看的那半句。
  const now = Date.now() / 1000;
  const days = Math.floor((at - now) / 86400);
  const remaining = at <= now ? "已过期" : days > 0 ? `剩 ${days} 天` : "不到 1 天";
  return {
    text: remaining + " · " + dateTime(at),
    warn: at <= now,
    hint: at <= now ? "凭据已过期，控制面下次签发时会换新" : "控制面给出的凭据过期时刻",
  };
});

// 公网 IP 与 NAT 类型来自 core 的 STUN 探测：多台机器各自看到自己的出口地址，
// 排查打洞失败时要看这一项。core 刚起来那几秒还没探完，如实显示"探测中"。
const publicIpText = computed(() => {
  const ips = snapshot.value?.publicIps ?? [];
  if (ips.length) return ips.join(" / ");
  return snapshot.value?.coreRunning ? "探测中" : "—";
});

const publicHint = computed(() => {
  const ips = snapshot.value?.publicIps ?? [];
  if (ips.length) return "STUN 报回来的本机公网出口地址";
  return snapshot.value?.coreRunning ? "core 正在向 STUN 查询出口地址" : "core 未运行";
});

const natHint = computed(() => {
  const udp = snapshot.value?.natTypeUdp || "未知";
  const tcp = snapshot.value?.natTypeTcp || "未知";
  return "STUN 判定的 NAT 类型（UDP / TCP 分别为 " + udp + " / " + tcp + "）；Unknown 表示探测未完成或 STUN 服务器不足两个";
});

const configUpdatedAt = computed(() => snapshot.value?.configUpdatedAt ?? 0);

// 侧栏窄，来源用两三个字母就够，中文全称会把这一列挤成竖排。
function sourceShort(source: string): string {
  return source === "app" ? "app" : "core";
}

// 后端已经筛成「客户端事件 + 内核告警」，这里直接用。
const recent = computed(() => logs.value);

const copied = ref("");
let copyTimer = 0;

async function copyIp(ip: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(ip);
    copied.value = ip;
    window.clearTimeout(copyTimer);
    copyTimer = window.setTimeout(() => {
      copied.value = "";
    }, 1200);
  } catch {
    setActionError("复制失败：浏览器拒绝了剪贴板访问");
  }
}

// 延迟条按最慢那个节点做归一，颜色只表达延迟（latencyClass）。
function barWidth(node: NodeRow): number {
  const ms = node.latencyMs;
  const max = latency.value?.max ?? 0;
  if (ms === null || !Number.isFinite(ms) || max <= 0) return 0;
  return Math.max(6, Math.round((ms / max) * 100));
}

// 排序状态只活在这一个页面里：不排序时直接用 remote 的原顺序（core 给的顺序），
// 不能先排一遍再按原顺序恢复，那样同值行的相对次序会被打乱。
const sorting = ref<NodeSort | null>(null);
const rows = computed(() =>
  sorting.value ? sortNodes(remote.value, sorting.value.key, sorting.value.direction) : remote.value,
);

function toggleSort(key: NodeSortKey): void {
  sorting.value = nextSort(sorting.value, key);
}

// 表头的可见指示：未排序是双向箭头，正序/倒序是单向箭头；同时给 th 一个 aria-sort。
function sortMark(key: NodeSortKey): string {
  if (sorting.value?.key !== key) return "↕";
  return sorting.value.direction === "asc" ? "↑" : "↓";
}

function ariaSort(key: NodeSortKey): "ascending" | "descending" | "none" {
  if (sorting.value?.key !== key) return "none";
  return sorting.value.direction === "asc" ? "ascending" : "descending";
}

// 颜色类名加前缀：页面里已有一个 `.unknown`（拓扑未知整块），裸用 unknown 会
// 让延迟格子继承它的 padding 与字号，把轨道撑成 28px 高。
function latencyClassOf(node: NodeRow): string {
  return "latency-" + latencyClass(node.latencyMs);
}

// 手动刷新：refresh(true) 会排在在途请求后面再拉一次，不等下一个 tick。
const refreshing = ref(false);

async function onRefresh(): Promise<void> {
  refreshing.value = true;
  try {
    await refresh(true);
  } finally {
    refreshing.value = false;
  }
}

async function onReconnect(): Promise<void> {
  try {
    await reconnect();
  } catch (err) {
    setActionError("重新连接失败: " + errorMessage(err));
  }
}

async function onCopy(): Promise<void> {
  const ip = snapshot.value?.localIp;
  if (!ip) return;
  try {
    await navigator.clipboard.writeText(ip);
  } catch {
    setActionError("复制失败：浏览器拒绝了剪贴板访问");
  }
}

// 面板地址就是当前页面，浏览器已经给了。
const panelHost = window.location.host;

// 顶部更新提示条：state 为空表示没有可更新的版本。
const update = computed(() => {
  const current = snapshot.value;
  if (!current || !current.updateState) return null;
  const name = current.updateVersion ? " " + current.updateVersion : "";
  if (current.updateState === "downloading") return { kind: "busy" as const, text: "正在下载新版本" + name + "…" };
  if (current.updateState === "ready") return { kind: "ready" as const, text: "新版本" + name + " 已就绪，更新后客户端会重启（网络中断几秒）" };
  // 失败态会一直在：退避到期自动重下一次、用户点重试、或服务端换了版本才会消失。
  // 所以这里看到 error 不是“等下一轮心跳就好”，是真失败了。
  return { kind: "error" as const, text: "新版本" + name + " 更新失败：" + current.updateError };
});

async function onApplyUpdate(): Promise<void> {
  if (!window.confirm("安装新版本并重启客户端？网络会中断几秒。")) return;
  await runApplyUpdate();
}

// 失败态的“重试”不一定是“装一个已经下好的包”：包没下完时后端会重新下一份，
// 下完才重启。确认框的文案要同时覆盖这两种情况。
async function onRetryUpdate(): Promise<void> {
  if (!window.confirm("重试更新？下载完成后客户端会重启，网络会中断几秒。")) return;
  await runApplyUpdate();
}

async function runApplyUpdate(): Promise<void> {
  try {
    await applyUpdate();
  } catch (err) {
    setActionError("更新失败: " + errorMessage(err));
  }
}
</script>

<template>
  <div class="view">
    <div v-if="restarting" class="alert busy">
      <span>正在安装新版本，完成后页面会自动刷新…</span>
    </div>
    <div v-if="update" class="alert" :class="update.kind">
      <span :title="update.text">{{ update.text }}</span>
      <button v-if="update.kind === 'ready'" type="button" class="primary" @click="onApplyUpdate">立即更新</button>
      <button v-if="update.kind === 'error'" type="button" @click="onRetryUpdate">重试</button>
    </div>

    <div class="dash">
      <div class="main">
        <section class="status">
          <div class="head">
            <h1 :class="status"><i class="dot" :class="status" />{{ statusText(status) }}</h1>
            <span class="chip" title="EasyTier 网络名">{{ snapshot?.networkName || "—" }}</span>
            <span class="addr">
              <span>{{ snapshot?.localIp || "—" }}</span>
              <i />
              <span>{{ snapshot?.hostname || "—" }}</span>
            </span>
          </div>
          <div class="actions">
            <button type="button" :disabled="refreshing" title="立刻拉一次数据，不等下一次轮询" @click="onRefresh">刷新</button>
            <button type="button" @click="onReconnect">重新连接</button>
            <button type="button" :disabled="!snapshot?.localIp" @click="onCopy">复制地址</button>
            <button type="button" class="primary" @click="select('settings')">打开设置</button>
          </div>
        </section>

        <section class="metrics">
          <div>
            <span class="k">在线节点</span>
            <span class="v">{{ known ? remote.length : "—" }}</span>
            <span class="n">{{ known ? "另含本机 1 台" : TOPOLOGY_UNKNOWN_NOTE }}</span>
          </div>
          <div>
            <span class="k">直连</span>
            <span class="v">{{ known ? directCount : "—" }}</span>
            <span class="n">{{ known ? "全部 1 跳" : TOPOLOGY_UNKNOWN_NOTE }}</span>
          </div>
          <div>
            <span class="k">中继</span>
            <span class="v" :class="{ warn: known && relayCount > 0 }">{{ known ? relayCount : "—" }}</span>
            <span class="n">{{ !known ? TOPOLOGY_UNKNOWN_NOTE : relayCount ? "存在绕行路径" : "全部直连" }}</span>
          </div>
          <div>
            <span class="k">平均延迟</span>
            <span class="v">{{ latency ? latencyNumber(latency.avg) : "—" }}<small v-if="latency">ms</small></span>
            <span class="n">{{ latency ? latencyRangeText(latency.min, latency.max) : known ? "暂无数据" : TOPOLOGY_UNKNOWN_NOTE }}</span>
          </div>
        </section>

        <section class="nodes">
          <header class="nodes-head">
            <h2>节点</h2>
            <span class="count">{{ nodeCount ?? "—" }}</span>
            <div class="subtabs">
              <button type="button" :class="{ active: nodeTab === 'list' }" @click="nodeTab = 'list'">列表</button>
              <button type="button" :class="{ active: nodeTab === 'topology' }" @click="nodeTab = 'topology'">拓扑</button>
            </div>
            <span class="spacer" />
            <span class="legend"><i class="sw direct" />直连 {{ known ? directCount : "—" }}<i class="sw relay" />中继 {{ known ? relayCount : "—" }}</span>
          </header>

          <div v-if="nodeTab === 'topology'" class="topo-wrap"><TopologyView /></div>

          <div v-else-if="!known" class="unknown">
            <strong>{{ TOPOLOGY_UNKNOWN_TEXT }}</strong>
            <span>{{ TOPOLOGY_UNKNOWN_HINT }}</span>
          </div>

          <table v-else>
            <thead>
              <tr>
                <th :aria-sort="ariaSort('name')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'name' }" title="点击排序：正序 → 倒序 → 默认" @click="toggleSort('name')">
                    节点<span class="arrow">{{ sortMark("name") }}</span>
                  </button>
                </th>
                <th class="addr" :aria-sort="ariaSort('ip')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'ip' }" title="点击排序：正序 → 倒序 → 默认" @click="toggleSort('ip')">
                    地址<span class="arrow">{{ sortMark("ip") }}</span>
                  </button>
                </th>
                <th class="col-route" :aria-sort="ariaSort('route')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'route' }" title="点击排序：直连 → 中继 → 未知，再倒序" @click="toggleSort('route')">
                    路径<span class="arrow">{{ sortMark("route") }}</span>
                  </button>
                </th>
                <th class="col-proto" :aria-sort="ariaSort('proto')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'proto' }" title="点击排序：正序 → 倒序 → 默认" @click="toggleSort('proto')">
                    协议<span class="arrow">{{ sortMark("proto") }}</span>
                  </button>
                </th>
                <th class="col-nat" :aria-sort="ariaSort('nat')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'nat' }" title="点击排序：正序 → 倒序 → 默认" @click="toggleSort('nat')">
                    NAT<span class="arrow">{{ sortMark("nat") }}</span>
                  </button>
                </th>
                <th class="col-ver" :aria-sort="ariaSort('version')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'version' }" title="点击排序：按版本号数值段比较" @click="toggleSort('version')">
                    core 版本<span class="arrow">{{ sortMark("version") }}</span>
                  </button>
                </th>
                <th class="num" :aria-sort="ariaSort('latency')">
                  <button type="button" class="sort" :class="{ active: sorting?.key === 'latency' }" title="点击排序：未知延迟始终排在最后" @click="toggleSort('latency')">
                    延迟<span class="arrow">{{ sortMark("latency") }}</span>
                  </button>
                </th>
              </tr>
            </thead>
            <tbody>
              <tr class="self">
                <td class="name">{{ snapshot?.hostname || "—" }}</td>
                <td class="addr">
                  <button
                    type="button"
                    class="ip"
                    :class="{ copied: copied === (localRow?.ip || snapshot?.localIp || '') }"
                    :title="copied === (localRow?.ip || snapshot?.localIp || '') ? '已复制' : '点击复制地址'"
                    @click="copyIp(localRow?.ip || snapshot?.localIp || '')"
                  >
                    {{ copied === (localRow?.ip || snapshot?.localIp || "") ? "已复制" : localRow?.ip || snapshot?.localIp || "—" }}
                  </button>
                </td>
                <td class="col-route"><span class="tag me">本机</span></td>
                <td class="col-proto">{{ joinOrDash(localRow?.tunnels) }}</td>
                <td class="col-nat" :title="'本机 STUN 判定（UDP）'">{{ localRow?.natType || snapshot?.natTypeUdp || "—" }}</td>
                <td class="col-ver">{{ localRow ? releaseVersion(localRow.version) : snapshot?.coreVersion || "—" }}</td>
                <td class="num"><span class="lat"><b class="latency-unknown">—</b></span></td>
              </tr>
              <tr v-for="node in rows" :key="node.ip">
                <td class="name">{{ node.hostname || node.ip }}</td>
                <td class="addr">
                  <button
                    type="button"
                    class="ip"
                    :class="{ copied: copied === node.ip }"
                    :title="copied === node.ip ? '已复制' : '点击复制地址'"
                    @click="copyIp(node.ip)"
                  >
                    {{ copied === node.ip ? "已复制" : node.ip }}
                  </button>
                </td>
                <td class="col-route"><span class="tag" :class="node.pathKind">{{ pathText(node.pathKind) }}</span></td>
                <td class="col-proto">{{ joinOrDash(node.tunnels) }}</td>
                <td class="col-nat">{{ node.natType || "—" }}</td>
                <td class="col-ver" :title="node.version">{{ releaseVersion(node.version) }}</td>
                <td class="num">
                  <span class="lat">
                    <b :class="latencyClassOf(node)">{{ latencyText(node.latencyMs) }}</b>
                    <i class="bar"><i :class="latencyClassOf(node)" :style="{ width: barWidth(node) + '%' }" /></i>
                  </span>
                </td>
              </tr>
            </tbody>
          </table>
          <div v-if="nodeTab === 'list' && known && !remote.length" class="empty">暂无节点数据。</div>
        </section>
      </div>

      <aside class="rail">
        <section>
          <header class="rail-head"><span>本机身份</span></header>
          <div class="identity">
          <h3 class="grp">网络</h3>
          <dl class="kv">
            <div><dt>内网 IP</dt><dd :title="snapshot?.localIp || ''">{{ snapshot?.localIp || "—" }}</dd></div>
            <div><dt>公网 IP</dt><dd :title="publicHint">{{ publicIpText }}</dd></div>
            <div><dt>NAT 类型</dt><dd :title="natHint">{{ snapshot?.natTypeUdp || "—" }}</dd></div>
          </dl>

          <h3 class="grp">身份</h3>
          <dl class="kv">
            <div><dt>设备名</dt><dd :title="snapshot?.hostname || ''">{{ snapshot?.hostname || "—" }}</dd></div>
            <div><dt>设备 ID</dt><dd :title="snapshot?.deviceId || ''">{{ snapshot?.deviceId || "—" }}</dd></div>
            <div><dt>认证方式</dt><dd :title="authHint">{{ authText }}</dd></div>
            <div v-if="credential">
              <dt>凭证过期</dt>
              <dd :class="{ warn: credential.warn }" :title="credential.hint">{{ credential.text }}</dd>
            </div>
          </dl>

          <h3 class="grp">运行</h3>
          <dl class="kv">
            <div><dt>core 版本</dt><dd :title="snapshot?.coreVersion || ''">{{ snapshot?.coreVersion || "—" }}</dd></div>
            <div><dt>core 进程</dt><dd>{{ snapshot?.coreRunning ? "运行中" : "未运行" }}</dd></div>
            <div><dt>已运行</dt><dd>{{ uptime }}</dd></div>
            <div><dt>最近心跳</dt><dd>{{ clockTime(snapshot?.lastHeartbeat ?? 0) }}</dd></div>
            <div><dt>服务端</dt><dd :title="snapshot?.serverUrl || ''">{{ serverHost || "—" }}</dd></div>
            <div><dt>配置 hash</dt><dd :title="snapshot?.configHash || ''">{{ snapshot?.configHash ? snapshot.configHash.slice(0, 12) : "—" }}</dd></div>
            <div><dt>配置更新</dt><dd title="config/config.toml 的最后修改时间">{{ dateTime(configUpdatedAt) }}</dd></div>
            <div v-if="snapshot?.encryption"><dt>加密算法</dt><dd title="配置 [flags] encryption_algorithm">{{ snapshot.encryption }}</dd></div>
            <div><dt>客户端</dt><dd>{{ snapshot?.appVersion || "—" }}</dd></div>
            <div><dt>面板</dt><dd :title="panelHost">{{ panelHost }}</dd></div>
            <div v-if="snapshot?.lastError" class="err">
              <dt>最后错误</dt>
              <dd :title="snapshot.lastError">{{ snapshot.lastError }}</dd>
            </div>
          </dl>
          </div>
        </section>

        <section class="events">
          <header class="rail-head">
            <span>最近事件</span>
            <button type="button" class="link" @click="select('logs')">全部日志</button>
          </header>
          <ul>
            <li v-for="(line, index) in recent" :key="index">
              <span class="t">{{ line.time }}</span>
              <span class="s">{{ sourceShort(line.source) }}</span>
              <span class="m" :title="line.text">{{ line.text }}</span>
              <span v-if="(line.count ?? 1) > 1" class="n" :title="'相邻的相同行已合并'">×{{ line.count }}</span>
            </li>
          </ul>
          <div v-if="!recent.length" class="empty">暂无事件。</div>
          <footer>每 1 秒轮询 · 心跳 {{ clockTime(snapshot?.lastHeartbeat ?? 0) }}</footer>
        </section>
      </aside>
    </div>
  </div>
</template>

<style scoped>
.view{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;overflow:hidden}

.alert{display:flex;align-items:center;gap:12px;padding:9px 24px;background:#eff6ff;border-bottom:1px solid #dbeafe;color:#1e40af;font-size:12.5px}
.alert.error{background:#fef2f2;border-bottom-color:#fecaca;color:var(--red)}
.alert span{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.alert button{flex:none;white-space:nowrap;margin-left:auto;padding:4px 12px;font-size:12px}

.dash{flex:1 1 auto;min-height:0;display:grid;grid-template-columns:minmax(0,1fr) 340px;overflow:hidden}
.main{min-height:0;overflow-y:auto;scrollbar-gutter:stable}

/* 状态区就是页面标题：状态 + 网络名 + 地址挤成一行，操作靠右，不再套卡片。 */
.status{display:flex;align-items:center;justify-content:space-between;gap:16px;flex-wrap:wrap;padding:13px 24px;border-bottom:1px solid var(--line-soft)}
.head{display:flex;align-items:center;gap:9px;min-width:0}
h1{display:flex;align-items:center;gap:7px;margin:0;font-size:15px;line-height:1.35;font-weight:500;color:var(--blue)}
h1 .dot{width:7px;height:7px}
h1.online{color:var(--green)}
h1.degraded{color:var(--orange)}
h1.error{color:var(--red)}
.chip{padding:1px 8px;border:1px solid var(--line);border-radius:999px;color:var(--muted);font-size:11px}
/* 状态行里的「地址 · 主机名」。选择器限定在 .status 内：表格的地址列也用 .addr，
   若在这里写光秃秃的 .addr，列会被改成 flex、列宽多出的一段就成了死空隙。 */
.status .addr{display:flex;align-items:center;gap:8px;font-size:12.5px;color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.status .addr i{width:2px;height:2px;border-radius:50%;background:var(--faint);flex:0 0 auto}
.actions{display:flex;gap:8px;flex:0 0 auto}

/* 指标行：四格由竖线分隔，没有卡片也没有底色。 */
.metrics{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));border-bottom:1px solid var(--line-soft)}
.metrics>div{display:flex;flex-direction:column;gap:2px;padding:14px 24px}
.metrics>div+div{border-left:1px solid var(--line-soft)}
.metrics .k{font-size:11px;color:var(--faint)}
.metrics .v{margin-top:1px;font-size:20px;line-height:1.25;font-weight:500}
.metrics .v small{margin-left:3px;font-size:11px;font-weight:400;color:var(--faint)}
.metrics .v.warn{color:var(--orange)}
.metrics .n{font-size:11px;color:var(--muted)}

.nodes{padding:18px 24px 26px}
.nodes-head{display:flex;align-items:center;gap:8px;margin-bottom:10px}

/* 节点区域的两个呈现：列表与拓扑图，用分段控件切换。 */
/* 分段控件：浅底 + 边框，选中项压成白底，才看得出当前是哪个视图。 */
.subtabs{display:flex;gap:2px;margin-left:6px;padding:2px;border:1px solid var(--line);border-radius:7px;background:var(--wash)}
.subtabs button{padding:2px 10px;border:0;border-radius:5px;background:transparent;color:var(--muted);font:inherit;font-size:11.5px;cursor:pointer}
.subtabs button:hover{color:var(--ink)}
.subtabs button.active{background:var(--card);color:var(--ink);font-weight:500;box-shadow:0 1px 2px rgba(15,23,42,.1)}
/* 拓扑图按固定高度嵌入，画布才有尺寸可算；宽度跟着主栏走。 */
.topo-wrap{display:flex;height:min(68vh,600px);border:1px solid var(--line);border-radius:9px;overflow:hidden}
.nodes-head h2{margin:0;font-size:13px;font-weight:500}
.count{font-size:11px;color:var(--faint)}
.spacer{flex:1 1 auto}
.legend{display:flex;align-items:center;gap:6px;font-size:11px;color:var(--muted)}
.sw{width:6px;height:6px;border-radius:50%}
.sw.direct{background:var(--green)}
.sw.relay{margin-left:8px;background:var(--orange)}

/* 节点表是真正的 table：列宽由内容撑开，余量由浏览器按比例摊掉，
   所以窗口变宽时不会出现某一列被拉成一大片空白。 */
table{width:100%;border-collapse:collapse;table-layout:auto}
th{padding:0 12px 7px;font-size:11px;font-weight:400;color:var(--faint);text-align:left;white-space:nowrap}
th.num{text-align:right}
/* 表头整格是排序按钮：未排序给双向箭头，正序/倒序给单向箭头并高亮当前列；
   三态循环见 format.ts 的 nextSort。 */
th .sort{display:inline-flex;align-items:center;gap:3px;padding:0;border:0;background:transparent;color:inherit;font:inherit;font-size:11px;cursor:pointer}
th .sort:hover{color:var(--ink)}
th .sort.active{color:var(--ink);font-weight:500}
th .sort .arrow{font-size:10px;line-height:1;color:var(--faint)}
th .sort.active .arrow{color:var(--blue)}
td{padding:9px 12px;vertical-align:middle}
th:first-child,td:first-child{padding-left:0}
th:last-child,td:last-child{padding-right:0}
/* 地址与延迟定宽：复制反馈把文字换成"已复制"时不重排整张表。 */
th.addr,td.addr{width:150px}
th.num,td.num{width:120px;text-align:right;padding-left:0}
tbody tr{border-top:1px solid var(--line-soft)}
tbody tr:hover{background:var(--wash)}
/* 本机一行用底线和下面的节点分开，其余行靠上边框分隔。 */
tbody tr.self{border-top:0;border-bottom:1px solid var(--line-soft)}
tbody tr.self:hover{background:transparent}

.name{max-width:240px;font-size:12.5px;font-weight:500;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}

/* 地址是个按钮：点一下就复制，反馈直接改写文字，不另起提示。 */
.ip{margin:0 -6px;padding:1px 6px;border:0;border-radius:4px;background:transparent;color:var(--muted);font:inherit;font-size:11.5px;font-family:ui-monospace,"Cascadia Mono",Consolas,monospace;cursor:pointer;white-space:nowrap}
.ip:hover{background:var(--line-soft);color:var(--ink)}
.ip.copied{color:var(--green)}

.col-proto,.col-nat,.col-ver{font-size:11.5px;color:var(--muted);white-space:nowrap}
.col-ver{width:96px}

/* 延迟的着色只按阈值（<50 绿、50–100 黄、≥100 红），与路径种类无关：
   直连也可能慢、中继也可能快，两件事分别由数字颜色和「路径」列的标签表达。 */
.lat{display:flex;flex-direction:column;align-items:flex-end;gap:4px}
.lat b{font-size:12.5px;font-weight:500;color:var(--faint)}
.lat b.latency-fast{color:var(--green)}
.lat b.latency-medium{color:#a16207}
.lat b.latency-slow{color:var(--red)}
/* 轨道与填充同高同圆角：固定 4px 比原来的 2px 稳，奇数高度或非整数宽度下
   也不会出现某些行看起来更粗。溢出裁掉，宽度仍按最大值归一。 */
.bar{width:100%;height:4px;border-radius:2px;background:var(--line-soft);overflow:hidden}
.bar i{display:block;height:100%;border-radius:2px;background:var(--faint);transition:width .3s}
.bar i.latency-fast{background:var(--green)}
.bar i.latency-medium{background:#a16207}
.bar i.latency-slow{background:var(--red)}

.tag{flex:0 0 auto;padding:1px 6px;border-radius:4px;font-size:10.5px;text-align:center;white-space:nowrap}
.tag.direct{color:var(--green);background:#f1f8f3}
.tag.relay{color:var(--orange);background:#fdf5ef}
.tag.me{color:var(--blue);background:#f1f5ff}
.tag.route{color:var(--muted);background:var(--wash)}

/* 右栏：本机身份在上、最近事件在下，两者用一条细线分隔，不套卡片。 */
.rail{display:flex;flex-direction:column;min-height:0;border-left:1px solid var(--line);background:var(--wash)}
.rail section{display:flex;flex-direction:column;min-height:0}
.rail-head{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:11px 14px;border-bottom:1px solid var(--line-soft);font-size:12px;font-weight:500}
.rail-head .link{padding:0;border:0;background:transparent;color:var(--blue);font:inherit;font-size:11px;cursor:pointer}

/* 身份项多了之后，窗口很矮时右栏放不下：让列表自己滚动，别把下面的事件挤没。 */
/* 身份分组整体滚动，按钮固定在底部，避免内容多了把它顶到文字上。 */
.identity{flex:1 1 auto;min-height:0;overflow-y:auto;padding-bottom:4px;scrollbar-gutter:stable}
.kv{display:grid;gap:5px;margin:0;padding:6px 14px 4px;font-size:11px}
/* 分组小标题：只做视觉分区，不抢正文的字重与颜色。 */
.grp{margin:10px 14px 0;font-size:10.5px;font-weight:500;color:var(--faint);letter-spacing:.04em}
.grp:first-of-type{margin-top:4px}
.kv>div{display:flex;gap:10px}
.kv dt{flex:0 0 56px;color:var(--faint)}
.kv dd{min-width:0;margin:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.kv dd.warn{color:var(--orange)}
.kv .err dt,.kv .err dd{color:var(--red)}

.rail button.danger{align-self:flex-start;margin:0 14px 12px;padding:4px 10px;border:1px solid #eec9c9;border-radius:6px;background:transparent;color:var(--red);font:inherit;font-size:11.5px;cursor:pointer}
.rail button.danger:hover{background:#fdf4f4}

.events{flex:1 1 auto;border-top:1px solid var(--line-soft)}
.events ul{flex:1 1 auto;min-height:0;overflow-y:auto;margin:0;padding:6px 0;list-style:none;font-size:11px;line-height:1.7;scrollbar-gutter:stable}
.events li{display:grid;grid-template-columns:44px 26px minmax(0,1fr) auto;gap:6px;padding:3px 12px}
.events .t,.events .s{color:var(--faint)}
.events .m{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:#3f4753}
.events .n{color:var(--faint);font-variant-numeric:tabular-nums}
.events footer{padding:8px 12px;border-top:1px solid var(--line-soft);font-size:10.5px;color:var(--faint)}

.dot{flex:0 0 auto;width:6px;height:6px;border-radius:50%;background:var(--faint)}
.dot.online{background:var(--green)}
.dot.connecting{background:var(--blue);animation:pulse 1.4s infinite}
.dot.degraded{background:var(--orange)}
.dot.error{background:var(--red)}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.3}}

.empty{padding:14px 2px;font-size:12px;color:var(--faint)}

/* 拓扑未知：只说清楚"查不到"，不摆空表，也不占整页当错误横幅。 */
.unknown{display:flex;flex-direction:column;gap:3px;padding:14px 2px;font-size:12px;color:var(--muted)}
.unknown strong{font-size:12.5px;font-weight:500;color:var(--ink)}

button{padding:5px 12px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12px;cursor:pointer}
button.primary{border-color:var(--blue);background:var(--blue);color:#fff}
button:hover:not(:disabled){background:var(--wash)}
button.primary:hover:not(:disabled){background:#1d4ed8}
button:disabled{opacity:.45;cursor:default}

@media(max-width:1100px){
  /* 窄窗口下右栏折到主区下面。身份项有十来行，再套一层固定高度只会把列表截成
     半行，所以这里改成一个滚动区：主区与右栏都按内容撑开，整页一起滚。 */
  .dash{display:flex;flex-direction:column;overflow-y:auto;scrollbar-gutter:stable}
  .main{flex:0 0 auto;overflow:visible}
  .rail{flex:0 0 auto;border-left:0;border-top:1px solid var(--line);max-height:none}
  .status{grid-template-columns:minmax(0,1fr);gap:20px}
  .col-proto,.col-nat,.col-ver{display:none}
  .name{max-width:200px}
}
@media(max-width:760px){
  .metrics{grid-template-columns:repeat(2,minmax(0,1fr))}
  .metrics>div:nth-child(3){border-left:0}
  .metrics>div:nth-child(n+3){border-top:1px solid var(--line-soft)}
  .col-route{display:none}
  .lat{width:96px}
}
</style>