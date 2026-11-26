<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useClient } from "../client";

const { logs } = useClient();
const box = ref<HTMLElement | null>(null);

type Filter = "all" | "app" | "core";

const FILTERS: { id: Filter; label: string }[] = [
  { id: "all", label: "全部" },
  { id: "app", label: "应用" },
  { id: "core", label: "内核" },
];

const filter = ref<Filter>("all");
const query = ref("");
const following = ref(true);
const clock = ref("");

// core 的输出也标成 core（历史上有过别的来源名，一并归到这里）。
function sourceOf(source: string): string {
  return source === "app" ? "app" : "core";
}

interface Row {
  time: string;
  source: string;
  text: string;
  /** 后端把相邻的重复行合并过，这里是合并掉的条数（1 表示没合并）。 */
  count: number;
}

// core 的多行输出（anyhow 的 Caused by 分支、listeners 数组）每一行都是独立记录，
// 逐行渲染会变成一列重复时间戳 + 孤零零的 ")"。以缩进开头、或只有一个收尾括号的行，
// 并回同一个块，按原文换行显示。
const merged = computed<Row[]>(() => {
  const out: Row[] = [];
  for (const line of logs.value) {
    const source = sourceOf(line.source);
    const last = out[out.length - 1];
    if (last && (line.text.startsWith(" ") || /^[)\]}]$/.test(line.text))) {
      last.text += "\n" + line.text;
      continue;
    }
    out.push({ time: line.time, source, text: line.text, count: line.count ?? 1 });
  }
  return out;
});

const counts = computed<Record<Filter, number>>(() => ({
  all: merged.value.length,
  app: merged.value.filter((row) => row.source === "app").length,
  core: merged.value.filter((row) => row.source === "core").length,
}));

const shown = computed(() => {
  const needle = query.value.trim().toLowerCase();
  return merged.value.filter((row) => {
    if (filter.value !== "all" && row.source !== filter.value) return false;
    if (needle && !row.text.toLowerCase().includes(needle)) return false;
    return true;
  });
});

// 级别只用来给整行加一条浅色底纹，不做行内着色：日志正文本身已经够密。
function level(text: string): string {
  if (/ERROR|FATAL/.test(text)) return "err";
  if (/WARN/.test(text)) return "warn";
  return "";
}

// 滚动条在底部就视为「跟随」，往上翻就停住——不做显式开关，省一行高度。
const BOTTOM_SLACK = 12;

function atBottom(el: HTMLElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_SLACK;
}

function scrollToBottom(): void {
  const el = box.value;
  if (el) el.scrollTop = el.scrollHeight;
}

function onScroll(): void {
  const el = box.value;
  if (el) following.value = atBottom(el);
}

function pinBottom(): void {
  following.value = true;
  scrollToBottom();
}

// 跟随与否要在更新前量（那时 scrollTop 还对着旧内容），滚动放到更新后做
// （那时 scrollHeight 才是新内容的高度）。用两个 watcher 把这两件事分开，
// 比在一个 async 回调里 await nextTick 更不容易踩时序。
watch(
  shown,
  () => {
    const el = box.value;
    if (el) following.value = atBottom(el);
  },
  { flush: "pre" },
);

watch(
  shown,
  () => {
    if (following.value) scrollToBottom();
  },
  { flush: "post" },
);

let timer = 0;

// 切到日志页时这里已经攒着上一轮的旧内容（甚至几十行），不主动贴底就会停在
// 顶部。挂载时先滚到底，之后交给上面的 watch 跟随。
onMounted(() => {
  scrollToBottom();
  const tick = (): void => {
    clock.value = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  };
  tick();
  timer = window.setInterval(tick, 1000);
});

onUnmounted(() => window.clearInterval(timer));
</script>

<template>
  <div class="view">
    <div class="bar">
      <div class="seg">
        <button
          v-for="item in FILTERS"
          :key="item.id"
          type="button"
          :class="{ on: filter === item.id }"
          @click="filter = item.id"
        >
          {{ item.label }}<span class="n">{{ counts[item.id] }}</span>
        </button>
      </div>
      <input v-model="query" class="q" type="search" placeholder="过滤日志内容" autocomplete="off" spellcheck="false" />
      <button type="button" class="follow" :class="{ on: following }" @click="pinBottom">
        <i class="dot" />{{ following ? "跟随中" : "回到最新" }}
      </button>
    </div>

    <div ref="box" class="logbox" @scroll="onScroll">
      <div v-for="(row, index) in shown" :key="index" class="lrow" :class="[row.source, level(row.text)]">
        <span class="lts">{{ row.time }}</span>
        <span class="lsrc">[{{ row.source }}]</span>
        <span class="ltx">{{ row.text }}</span>
      </div>
      <div v-if="!shown.length" class="empty">{{ logs.length ? "没有匹配的日志" : "暂无日志" }}</div>
    </div>

    <footer class="foot">
      <span>共 {{ shown.length }} 行</span>
      <template v-if="query.trim()">
        <span class="dim">·</span>
        <span>筛自 {{ logs.length }}</span>
      </template>
      <span class="dim">·</span>
      <span>[app] 客户端事件 · [core] easytier-core 输出</span>
      <span class="spacer" />
      <span class="dim">{{ clock }}</span>
    </footer>
  </div>
</template>

<style scoped>
.view{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;overflow:hidden}

/* 工具条只放三件事：来源分段、内容过滤、是否跟随。 */
.bar{display:flex;align-items:center;gap:12px;padding:10px 16px;border-bottom:1px solid var(--line-soft)}
.seg{display:flex;gap:2px;padding:2px;border-radius:7px;background:var(--bg)}
.seg button{display:inline-flex;align-items:center;gap:5px;padding:3px 10px;border:0;border-radius:6px;background:transparent;color:var(--muted);font:inherit;font-size:12px;cursor:pointer}
.seg button.on{background:var(--card);color:var(--ink);box-shadow:0 0 0 1px var(--line)}
.seg .n{font-size:11px;color:var(--faint)}
.seg button.on .n{color:var(--muted)}

.q{max-width:320px;flex:1 1 auto;padding:5px 10px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12px;outline:none}
.q:focus{border-color:var(--blue)}
.q::placeholder{color:var(--faint)}

.follow{display:inline-flex;align-items:center;gap:6px;margin-left:auto;padding:4px 10px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--muted);font:inherit;font-size:11.5px;white-space:nowrap;cursor:pointer}
.follow .dot{width:6px;height:6px;border-radius:50%;background:var(--faint)}
.follow.on{color:var(--green)}
.follow.on .dot{background:var(--green);animation:pulse 1.6s infinite}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.35}}

/* 日志是唯一用等宽字体的地方：字符网格本身就是信息。左边缘那条 2px 的线
   只在告警行出现，正常行留透明占位，免得两种行宽度不一致。 */
/* 续行按原文换行显示，行内边距给 2px，让多行块读起来是一段而不是几行。 */
.logbox{flex:1 1 auto;min-height:0;overflow-y:auto;padding:8px 0;font:12px/1.65 var(--mono);color:#1f2937;scrollbar-gutter:stable}
.lrow{display:grid;grid-template-columns:58px 46px minmax(0,1fr);gap:8px;align-items:baseline;padding:2px 16px;border-left:2px solid transparent}
.lrow:hover{background:var(--wash)}
.lrow.warn{background:#fffdf4;border-left-color:#dda94b}
.lrow.err{background:#fff7f6;border-left-color:var(--red)}
.lts{color:var(--faint)}
.lsrc{font-size:11px;color:var(--blue)}
.lrow.core .lsrc{color:var(--faint)}
.ltx{white-space:pre-wrap;word-break:break-all}
.empty{padding:14px 16px;font:12px/1.55 var(--sans);color:var(--faint)}

.foot{flex:0 0 auto;display:flex;align-items:center;gap:6px;padding:7px 16px;border-top:1px solid var(--line);background:var(--wash);font-size:11px;color:var(--muted)}
.foot .dim{color:var(--faint)}
.foot .spacer{flex:1 1 auto}

@media(max-width:760px){
  .bar{flex-wrap:wrap;gap:8px}
  .q{max-width:none;order:3;flex-basis:100%}
  .logbox{font-size:11.5px}
  .lrow{grid-template-columns:52px 42px minmax(0,1fr);padding:2px 12px}
}
</style>
