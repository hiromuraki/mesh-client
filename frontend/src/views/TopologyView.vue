<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useClient } from "../client";
import {
  joinOrDash,
  latencyText,
  pathText,
  routeText,
  TOPOLOGY_UNKNOWN_HINT,
  TOPOLOGY_UNKNOWN_TEXT,
} from "../format";
import type { NodeRow } from "../api/types";

const { nodes, snapshot } = useClient();

// 画布用固定坐标再整体缩放，节点位置与窗口大小无关。
const W = 960;
const H = 620;
const CX = W / 2;
const CY = H / 2;
const BOX_W = 148;
const BOX_H = 52;

// 勾选集合默认全选：五六个节点时一眼看全，多了再取消。
const picked = ref(new Set<string>());
const focused = ref("");
let seeded = false;

// 节点表为 null 表示拓扑未知（core 在跑但查询失败）：它与「core 未运行、
// 一台节点都没有」是两件事，界面要出提示而不是空图。
const known = computed(() => nodes.value !== null);

// 本机由中间的固定框表示，不参与勾选与环形排布。
const candidates = computed(() => (nodes.value ?? []).filter((node) => node.pathKind !== "me"));

const pickedList = computed(() => candidates.value.filter((node) => picked.value.has(node.ip)));

// 头一两帧数据还是空的，等真拿到节点再落默认全选，否则会被空列表清掉。
watch(
  candidates,
  (list) => {
    if (seeded || !list.length) return;
    seeded = true;
    picked.value = new Set(list.map((node) => node.ip));
  },
  { immediate: true },
);

function isPicked(ip: string): boolean {
  return picked.value.has(ip);
}

function toggle(ip: string): void {
  const next = new Set(picked.value);
  if (next.has(ip)) next.delete(ip);
  else next.add(ip);
  picked.value = next;
}

const allPicked = computed(() => candidates.value.length > 0 && pickedList.value.length === candidates.value.length);

function toggleAll(): void {
  picked.value = allPicked.value ? new Set() : new Set(candidates.value.map((node) => node.ip));
}

interface Placed {
  node: GNode;
  x: number;
  y: number;
  depth: number;
}

interface Edge {
  key: string;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  relay: boolean;
  /** 路径只画到已知的那一段：这段之后链就断了，不能当成整条。 */
  broken: boolean;
  label: string;
  lx: number;
  ly: number;
}

function edge(
  key: string,
  x1: number,
  y1: number,
  x2: number,
  y2: number,
  relay: boolean,
  broken: boolean,
  label: string,
): Edge {
  return { key, x1, y1, x2, y2, relay, broken, label, lx: (x1 + x2) / 2, ly: (y1 + y2) / 2 };
}

// 按 IP 排序再排布：心跳换一次顺序不能让整张图重排。
function ipKey(ip: string): number {
  const parts = ip.split(".").map(Number);
  if (parts.length !== 4) return 0;
  const [a, b, c, d] = parts as [number, number, number, number];
  if (![a, b, c, d].every(Number.isFinite)) return 0;
  return a * 16777216 + b * 65536 + c * 256 + d;
}

// 每一层一个椭圆环；层数等于中间跳的个数加一。横轴拉长是补偿框比圆宽。
function ringX(depth: number): number {
  return 300 + (depth - 1) * 240;
}
function ringY(depth: number): number {
  return 196 + (depth - 1) * 158;
}

// 图里的一个位置：真节点用它自己的数据，中间跳是为画图造出来的占位节点
// （hop=true），它们只负责把链路补完整，不能点开详情、也不参与勾选。
interface GNode {
  ip: string;
  hostname: string;
  hop: boolean;
  row: NodeRow | null;
}

// 位置由路径决定：直接可达的节点在内环，经 k 跳的落在第 k+1 层，
// 角度取父节点扇区的中心，所以连线永远是短的、向外的。
// 路径上的中间节点即使没被勾选也会补进来——中继必须画出来，否则一条虚线
// 标着「经 ali-seed」却说不出它到底接在哪。
const tree = computed(() => {
  const list = [...pickedList.value].sort((a, b) => ipKey(a.ip) - ipKey(b.ip));
  const parent = new Map<string, string>();
  if (!list.length) return { placed: [] as Placed[], parent };

  const nodes = new Map<string, GNode>();
  const children = new Map<string, string[]>();
  const link = (ip: string, owner: string): void => {
    parent.set(ip, owner);
    const bucket = children.get(owner);
    if (bucket) {
      if (!bucket.includes(ip)) bucket.push(ip);
    } else {
      children.set(owner, [ip]);
    }
  };

  for (const node of list) {
    nodes.set(node.ip, { ip: node.ip, hostname: node.hostname || node.ip, hop: false, row: node });
    const chain = (node.path ?? []).map((hop) => hop.ip);
    let owner = "";
    for (const hop of node.path ?? []) {
      if (!nodes.has(hop.ip)) {
        nodes.set(hop.ip, { ip: hop.ip, hostname: hop.hostname || hop.ip, hop: true, row: null });
      }
      link(hop.ip, owner);
      owner = hop.ip;
    }
    link(node.ip, owner);
    void chain;
  }

  const weight = new Map<string, number>();
  const weigh = (ip: string): number => {
    const cached = weight.get(ip);
    if (cached !== undefined) return cached;
    let total = 1;
    for (const kid of children.get(ip) ?? []) total += weigh(kid);
    weight.set(ip, total);
    return total;
  };

  const placed: Placed[] = [];
  const place = (owner: string, start: number, end: number, depth: number): void => {
    const kids = children.get(owner) ?? [];
    if (!kids.length) return;
    const total = kids.reduce((sum, kid) => sum + weigh(kid), 0);
    let cursor = start;
    for (const kid of kids) {
      const node = nodes.get(kid);
      if (!node) continue;
      const span = ((end - start) * weigh(kid)) / total;
      // 整棵树的唯一子节点摆在正上方，否则取扇区中心。
      const angle = owner === "" && kids.length === 1 ? -Math.PI / 2 : cursor + span / 2;
      placed.push({
        node,
        depth,
        x: CX + ringX(depth) * Math.cos(angle),
        y: CY + ringY(depth) * Math.sin(angle),
      });
      place(kid, cursor, cursor + span, depth + 1);
      cursor += span;
    }
  };
  place("", -Math.PI / 2, (3 * Math.PI) / 2, 1);
  return { placed, parent };
});

// 中继除了"经过谁"还要看行本身：路径只走到半路时 path 可能一段都没有，
// 这一行仍然是 relay。
function isRelay(node: GNode): boolean {
  return node.hop || node.row?.pathKind === "relay" || (node.row?.path?.length ?? 0) > 0;
}

// 每条边只画一次：从父节点到子节点。父节点不在图里时退回本机直连。
//
// 路径只走到半路（pathIncomplete）时，已经走到的那一段是真的，照常画出来；但
// 通向目标的那一小段画成虚线并标注，说明链到此为止——否则图上会把半截链当成
// 整条路径。中间跳是为画链路补出来的占位节点，不看它们的 pathIncomplete。
const edges = computed<Edge[]>(() => {
  const { placed, parent } = tree.value;
  const pos = new Map(placed.map((item) => [item.node.ip, item]));
  const out: Edge[] = [];
  for (const item of placed) {
    const ownerIp = parent.get(item.node.ip) ?? "";
    const owner = ownerIp ? pos.get(ownerIp) : undefined;
    const relay = isRelay(item.node);
    const broken = item.node.row?.pathIncomplete === true;
    if (owner) {
      out.push(edge(item.node.ip, owner.x, owner.y, item.x, item.y, relay, broken, broken ? "路径不完整" : ""));
      continue;
    }
    // 中间跳一定会被补进图里，只有数据自相矛盾时才会退回这条带标签的直连。
    const hop = item.node.row?.path?.at(-1) ?? null;
    out.push(
      edge(
        "root-" + item.node.ip,
        CX,
        CY,
        item.x,
        item.y,
        relay,
        broken,
        broken ? "路径不完整" : hop ? "经 " + (hop.hostname || hop.ip) : "",
      ),
    );
  }
  return out;
});

// 层数多了就整体缩小，而不是把外环裁掉。
const canvas = computed(() => {
  const items = tree.value.placed;
  if (!items.length) return { x: 0, y: 0, w: W, h: H };
  let minX = CX - BOX_W / 2;
  let maxX = CX + BOX_W / 2;
  let minY = CY - BOX_H / 2;
  let maxY = CY + BOX_H / 2;
  for (const item of items) {
    minX = Math.min(minX, item.x - BOX_W / 2);
    maxX = Math.max(maxX, item.x + BOX_W / 2);
    minY = Math.min(minY, item.y - BOX_H / 2);
    maxY = Math.max(maxY, item.y + BOX_H / 2);
  }
  const w = Math.max(W, maxX - minX + 72);
  const h = Math.max(H, maxY - minY + 72);
  return { x: (minX + maxX - w) / 2, y: (minY + maxY - h) / 2, w, h };
});

const focusedNode = computed(() => (nodes.value ?? []).find((node) => node.ip === focused.value) ?? null);

const focusedRoute = computed(() => (focusedNode.value ? routeText(focusedNode.value) : "—"));

const directCount = computed(() => pickedList.value.filter((node) => node.pathKind === "direct").length);
const relayCount = computed(() => pickedList.value.filter((node) => node.pathKind === "relay").length);
</script>

<template>
  <div class="topo">
    <aside class="picker">
      <div class="picker-head">
        <span>节点 {{ known ? candidates.length : "—" }} 个</span>
        <button type="button" @click="toggleAll">{{ allPicked ? "全不选" : "全选" }}</button>
      </div>
      <div class="list">
        <label
          v-for="node in candidates"
          :key="node.ip"
          class="item"
          :class="{ on: isPicked(node.ip), active: focused === node.ip }"
          @click="focused = node.ip"
        >
          <input type="checkbox" :checked="isPicked(node.ip)" @change="toggle(node.ip)" />
          <span class="body">
            <span class="line1">
              <span class="name">{{ node.hostname || node.ip }}</span>
              <span class="lat">{{ latencyText(node.latencyMs) }}</span>
            </span>
            <span class="line2">
              <span class="ip">{{ node.ip }}</span>
              <span class="tag" :class="node.pathKind">{{ pathText(node.pathKind) }}</span>
            </span>
          </span>
        </label>
        <div v-if="!candidates.length" class="empty">{{ known ? "暂无节点数据。" : TOPOLOGY_UNKNOWN_TEXT }}</div>
      </div>
    </aside>

    <section class="canvas">
      <div v-if="!known" class="empty-state">
        <strong>{{ TOPOLOGY_UNKNOWN_TEXT }}</strong>
        <p>{{ TOPOLOGY_UNKNOWN_HINT }}</p>
      </div>

      <div v-else-if="!pickedList.length" class="empty-state">
        <strong>还没有选中节点</strong>
        <p>在左侧勾选节点，这里会画出它们到本机的路径。</p>
        <button v-if="candidates.length" type="button" @click="toggleAll">全选 {{ candidates.length }} 个节点</button>
      </div>

      <svg v-else :viewBox="`${canvas.x} ${canvas.y} ${canvas.w} ${canvas.h}`" preserveAspectRatio="xMidYMid meet" role="img" aria-label="节点拓扑">
        <line
          v-for="line in edges"
          :key="line.key"
          :x1="line.x1"
          :y1="line.y1"
          :x2="line.x2"
          :y2="line.y2"
          :class="{ relay: line.relay, broken: line.broken }"
          class="edge"
        />

        <g v-for="line in edges.filter((e) => e.label)" :key="line.key + '-label'" class="edge-label">
          <rect :x="line.lx - 34" :y="line.ly - 9" width="68" height="18" rx="9" />
          <text :x="line.lx" :y="line.ly + 4" text-anchor="middle">{{ line.label }}</text>
        </g>

        <g class="self">
          <rect :x="CX - BOX_W / 2" :y="CY - BOX_H / 2" :width="BOX_W" :height="BOX_H" rx="8" />
          <text class="n1" :x="CX - BOX_W / 2 + 12" :y="CY + 1">{{ snapshot?.hostname || "本机" }}</text>
          <text class="n2" :x="CX - BOX_W / 2 + 12" :y="CY + 18">{{ snapshot?.localIp || "—" }}</text>
          <text class="n3" :x="CX + BOX_W / 2 - 12" :y="CY + 18" text-anchor="end">本机</text>
        </g>

        <g
          v-for="item in tree.placed"
          :key="item.node.ip"
          class="node"
          :class="[item.node.hop ? 'hop' : item.node.row?.pathKind, { focused: focused === item.node.ip }]"
          :transform="`translate(${item.x - BOX_W / 2}, ${item.y - BOX_H / 2})`"
          @click="item.node.hop ? null : (focused = item.node.ip)"
        >
          <rect :width="BOX_W" :height="BOX_H" rx="8" />
          <text class="n1" x="12" y="22">{{ item.node.hostname || item.node.ip }}</text>
          <text class="n2" x="12" y="40">{{ item.node.ip }}</text>
          <text class="n3" :x="BOX_W - 12" y="40" text-anchor="end">{{ item.node.hop ? "中继" : latencyText(item.node.row?.latencyMs) }}</text>
        </g>
      </svg>

      <footer v-if="known" class="legend">
        <span class="key direct">直连</span>
        <span class="key relay">中继</span>
        <span class="key broken">路径不完整</span>
        <span class="sep">·</span>
        <span>已选 {{ pickedList.length }} / {{ candidates.length }} · 直连 {{ directCount }} · 中继 {{ relayCount }}</span>
      </footer>

      <div v-if="focusedNode" class="detail">
        <div class="detail-head">
          <strong>{{ focusedNode.hostname || focusedNode.ip }}</strong>
          <button type="button" @click="focused = ''">×</button>
        </div>
        <dl>
          <dt>地址</dt><dd>{{ focusedNode.ip || "—" }}</dd>
          <dt>路径</dt><dd :title="focusedRoute">{{ focusedRoute }}</dd>
          <dt>延迟</dt><dd>{{ latencyText(focusedNode.latencyMs) }}</dd>
          <dt>协议</dt><dd>{{ joinOrDash(focusedNode.tunnels) }}</dd>
          <dt>NAT</dt><dd>{{ focusedNode.natType || "—" }}</dd>
          <dt>跳数</dt><dd>{{ focusedNode.hops || "—" }}</dd>
        </dl>
      </div>
    </section>
  </div>
</template>

<style scoped>
.topo{flex:1 1 auto;min-height:0;display:grid;grid-template-columns:264px minmax(0,1fr);overflow:hidden}

.picker{display:flex;flex-direction:column;min-height:0;border-right:1px solid var(--line);background:var(--wash)}
.picker-head{display:flex;align-items:center;justify-content:space-between;padding:11px 14px;border-bottom:1px solid var(--line);font-size:12px;color:var(--muted)}
.picker-head button{padding:3px 10px;font-size:11px}
.list{flex:1 1 auto;min-height:0;overflow-y:auto;padding:6px;scrollbar-gutter:stable}

.item{display:flex;align-items:flex-start;gap:8px;padding:8px 8px;border-radius:7px;cursor:pointer}
.item:hover{background:var(--line-soft)}
.item.active{background:#f1f5ff;box-shadow:inset 0 0 0 1px #dbe6ff}
.item input{margin:3px 0 0;accent-color:var(--blue);flex:0 0 auto}
.body{display:flex;flex-direction:column;gap:2px;min-width:0;flex:1 1 auto}
.line1,.line2{display:flex;align-items:center;gap:8px;min-width:0}
.name{font-size:12.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lat{margin-left:auto;font-size:11px;color:var(--muted);flex:0 0 auto}
.ip{font-size:11px;color:var(--muted)}
.tag{margin-left:auto;font-size:10.5px;padding:0 6px;border-radius:999px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:120px}
.tag.direct{color:var(--green);background:#f1f8f3}
.tag.relay{color:var(--orange);background:#fdf5ef}
.tag.me{color:var(--blue);background:#f1f5ff}
.tag.route{color:var(--muted);background:var(--line-soft)}
.empty{padding:13px 8px;font-size:12px;color:var(--muted)}

.canvas{position:relative;min-width:0;display:flex;flex-direction:column;background:var(--card)}
svg{flex:1 1 0;min-height:0;width:100%;display:block;padding:8px 12px 0}

.edge{stroke:#d7dde5;stroke-width:1.4}
.edge.relay{stroke:var(--orange);stroke-dasharray:5 4;opacity:.75}
/* 链只走到这里：点线比中继的虚线更"断"，并配一条「路径不完整」的边标签。 */
.edge.broken{stroke:var(--faint);stroke-dasharray:2 3;opacity:.9}

.edge-label rect{fill:#fff;stroke:var(--line-soft)}
.edge-label text{font-size:10.5px;fill:var(--muted)}

.self rect{fill:#f1f5ff;stroke:#c7d7f7;stroke-width:1.2}
.node{cursor:pointer}
.node rect{fill:#fff;stroke:var(--line);stroke-width:1.2}
.node:hover rect{stroke:#93c5fd}
.node.focused rect{stroke:var(--blue);stroke-width:2}
.node.relay rect{stroke:#e2c7a8}
/* 中间跳是为画链路补出来的占位节点：虚线框、不可点，和真正的节点区分开。 */
.node.hop{cursor:default}
.node.hop rect{fill:var(--wash);stroke:#cbd5e1;stroke-dasharray:3 3}
.node.hop .n1{fill:var(--muted);font-weight:400}
.node.hop .n3{fill:var(--orange)}

.self .n1,.node .n1{font-size:12px;fill:var(--ink);font-weight:500}
.self .n2,.node .n2{font-size:10.5px;fill:var(--faint)}
.self .n3,.node .n3{font-size:10.5px;fill:var(--muted)}

/* 一个都没勾选时给一块正经的空状态，而不是把本机摆在空画布正中。 */
.empty-state{flex:1 1 auto;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:6px;padding:24px;text-align:center}
.empty-state strong{font-size:13px;font-weight:500;color:var(--ink)}
.empty-state p{margin:0;font-size:12px;color:var(--muted)}
.empty-state button{margin-top:10px;padding:4px 12px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12px;cursor:pointer}
.empty-state button:hover{background:var(--wash)}

.legend{flex:0 0 auto;display:flex;align-items:center;flex-wrap:wrap;gap:8px;padding:7px 18px 11px;font-size:11px;color:var(--muted)}
.legend .key{display:inline-flex;align-items:center;gap:5px}
.legend .key::before{content:"";width:18px;height:0;border-top-width:2px;border-top-style:solid}
.legend .key.direct::before{border-top-color:#cbd5e1}
.legend .key.relay::before{border-top-color:var(--orange);border-top-style:dashed}
.legend .key.broken::before{border-top-color:var(--faint);border-top-style:dotted}
.legend .sep{opacity:.5}

.detail{position:absolute;right:16px;bottom:44px;width:212px;padding:10px 12px;border:1px solid var(--line);border-radius:8px;background:var(--card)}
.detail-head{display:flex;align-items:center;margin-bottom:7px;font-size:12.5px}
.detail-head button{margin-left:auto;border:0;background:transparent;color:var(--muted);font-size:15px;line-height:1;cursor:pointer;padding:0 2px}
.detail dl{display:grid;grid-template-columns:auto 1fr;gap:3px 10px;margin:0;font-size:11.5px}
.detail dt{color:var(--muted)}
.detail dd{margin:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}

button{border:1px solid var(--line);background:var(--card);color:var(--ink);border-radius:6px;font:inherit;cursor:pointer}
button:hover{filter:brightness(.97)}

@media(max-width:820px){
  .topo{grid-template-columns:1fr;grid-template-rows:minmax(0,200px) minmax(0,1fr)}
  .picker{border-right:0;border-bottom:1px solid var(--line)}
}
</style>
