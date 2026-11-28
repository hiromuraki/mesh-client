<script setup lang="ts">
import { onMounted, onUnmounted } from "vue";
import DataView from "./views/DataView.vue";
import LogsView from "./views/LogsView.vue";
import SettingsView from "./views/SettingsView.vue";
import { refresh, select, useClient } from "./client";
import { statusText } from "./format";
import type { View } from "./api/types";

const { view, status, banner } = useClient();

const TABS: { id: View; label: string }[] = [
  { id: "data", label: "数据" },
  { id: "logs", label: "日志" },
  { id: "settings", label: "设置" },
];

const POLL_MS = 1000;
let timer: number | undefined;

function stopPolling(): void {
  if (timer !== undefined) {
    window.clearInterval(timer);
    timer = undefined;
  }
}

function startPolling(): void {
  stopPolling();
  if (document.hidden) return;
  timer = window.setInterval(() => void refresh(), POLL_MS);
}

// 面板在浏览器里，标签页切到后台时停掉轮询，回到前台立刻刷新一次。
function onVisibilityChange(): void {
  if (document.hidden) {
    stopPolling();
    return;
  }
  void refresh();
  startPolling();
}

onMounted(() => {
  document.addEventListener("visibilitychange", onVisibilityChange);
  void refresh();
  startPolling();
});

onUnmounted(() => {
  document.removeEventListener("visibilitychange", onVisibilityChange);
  stopPolling();
});
</script>

<template>
  <div class="shell">
    <header class="topbar">
      <div class="brand">
        <span class="mark">
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path d="M12 4.2 19.4 18H4.6Z" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round" />
            <g fill="currentColor">
              <circle cx="12" cy="4.2" r="2.2" />
              <circle cx="19.4" cy="18" r="2.2" />
              <circle cx="4.6" cy="18" r="2.2" />
              <circle cx="12" cy="13.4" r="1.7" />
            </g>
          </svg>
        </span>
        <span class="name">Mesh</span>
      </div>

      <nav class="tabs">
        <button
          v-for="tab in TABS"
          :key="tab.id"
          type="button"
          class="tab"
          :class="{ active: view === tab.id }"
          @click="select(tab.id)"
        >
          {{ tab.label }}
        </button>
      </nav>

      <div class="status" :class="status">
        <span class="dot" :class="status"></span>
        <span>{{ statusText(status) }}</span>
      </div>
    </header>

    <div v-if="banner" class="banner">{{ banner }}</div>

    <main class="body">
      <DataView v-if="view === 'data'" />
      <LogsView v-else-if="view === 'logs'" />
      <SettingsView v-else />
    </main>
  </div>
</template>

<style scoped>
.shell{display:flex;flex-direction:column;height:100%;overflow:hidden}

.topbar{flex:0 0 auto;display:flex;align-items:center;gap:14px;height:46px;padding:0 14px;background:var(--card);border-bottom:1px solid var(--line)}

/* 标题区：图标 + 名称，和导航之间用一条竖线隔开，避免两个控件贴在一起。 */
.brand{display:flex;align-items:center;gap:8px}
.mark{display:flex;align-items:center;justify-content:center;width:22px;height:22px;border-radius:6px;background:#f1f5ff;color:var(--blue)}
.mark svg{width:15px;height:15px;display:block}
.name{font-size:12.5px;font-weight:500;letter-spacing:.01em}
.brand::after{content:"";width:1px;height:16px;margin-left:6px;background:var(--line)}

.tabs{display:flex;height:100%}
.tab{position:relative;height:100%;padding:0 11px;border:0;background:transparent;color:var(--muted);font:inherit;font-size:12.5px;cursor:pointer}
.tab:hover{color:var(--ink)}
.tab.active{color:var(--ink);font-weight:500}
.tab.active::after{content:"";position:absolute;left:11px;right:11px;bottom:-1px;height:2px;background:var(--blue)}

/* 连接状态收成右侧一枚描边小胶囊：圆点给一眼可辨的颜色，文字补足语义。 */
.status{display:flex;align-items:center;gap:6px;margin-left:auto;padding:3px 9px;border:1px solid var(--line);border-radius:999px;color:var(--muted);font-size:11.5px}
.status.online{border-color:#d8e9dd;background:#f6fbf7;color:#3f6b4d}
.status.error{border-color:#f3cfcf;background:#fdf6f6;color:var(--red)}
.dot{width:6px;height:6px;border-radius:50%;background:var(--faint)}
.dot.online{background:var(--green)}
.dot.connecting{background:var(--blue);animation:pulse 1.4s infinite}
.dot.degraded{background:var(--orange)}
.dot.error{background:var(--red)}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.3}}

.banner{padding:8px 18px;border-bottom:1px solid #fecaca;background:#fef2f2;color:var(--red);font-size:12px}

.body{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;overflow:hidden;background:var(--card)}
</style>
