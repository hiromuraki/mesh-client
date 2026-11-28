<script setup lang="ts">
import { computed, onUnmounted, reactive, ref, watch } from "vue";
import { resetConfig, saveSettings, setAutostart, useClient } from "../client";
import { clockTime, errorMessage } from "../format";

const { snapshot } = useClient();

const DEFAULT_SERVER_URL = "http://127.0.0.1:8088";

const form = reactive({ serverUrl: "", hostname: "" });
const message = reactive({ text: "", kind: "" as "" | "ok" | "error" });
const saving = ref(false);
const copied = ref(false);
const copyError = ref("");

// 表单只回填一次：之后由用户输入驱动，每秒一次的轮询不会覆盖正在输入的内容。
let filled = false;

function fill(next: { serverUrl: string; hostname: string } | null): void {
  if (!next) return;
  form.serverUrl = next.serverUrl || DEFAULT_SERVER_URL;
  form.hostname = next.hostname;
}

watch(
  snapshot,
  (current) => {
    if (!current || filled) return;
    fill({ serverUrl: current.serverUrl, hostname: current.hostnameSet });
    filled = true;
  },
  { immediate: true },
);

// ---- 状态区 ----

// 结论互斥：读取中 / 没配置 / 正在连 / 连上了 / 控制面拒绝 / 本机错误 / 连接异常。
type Phase = "loading" | "unconfigured" | "connecting" | "online" | "rejected" | "degraded" | "error";

// 控制面拒绝没有独立字段，原因落在 lastError 里（例如 401 unauthorized）。
// 本机故障（配置读不出来、core 起不来）也写 lastError，但状态多半已经是 error；
// 所以只有状态不是 error、或错误原文本身就指向认证时，才判成「控制面拒绝」。
const AUTH_ERROR = /401|403|unauthorized|forbidden|不认这把钥匙/i;

const phase = computed<Phase>(() => {
  const current = snapshot.value;
  if (!current) return "loading";
  if (current.status === "unconfigured" || !current.status) return "unconfigured";
  if (current.status === "online") return "online";
  if (current.lastError) {
    const localFault = current.status === "error" && !AUTH_ERROR.test(current.lastError);
    return localFault ? "error" : "rejected";
  }
  if (current.status === "error") return "error";
  if (current.status === "connecting") return "connecting";
  return "degraded";
});

const PHASE_TITLE: Record<Phase, string> = {
  loading: "正在读取状态",
  unconfigured: "未配置",
  connecting: "正在连接",
  online: "已连接",
  rejected: "被控制面拒绝",
  degraded: "连接异常",
  error: "本地错误",
};

const title = computed(() => PHASE_TITLE[phase.value]);

const detail = computed(() => {
  switch (phase.value) {
    case "loading":
      return "正在读取客户端状态。";
    case "unconfigured":
      return "本机还没有配置。先在控制面「新建设备」里填好下面这把公钥，再填地址保存。";
    case "connecting":
      return "配置已保存，正在向控制面发心跳；控制面的拒绝原因会在几秒内出现在这里。";
    case "online":
      return "控制面认可这把公钥，心跳正常。";
    case "rejected":
      return "控制面不接受这台设备的身份，连接没有建立。";
    case "degraded":
      return "配置已保存，但连接还没建立：控制面或 core 有一边没就绪，下一轮轮询会继续尝试。";
    case "error":
      return "本机这一侧出错，连接没有建立。";
  }
  return "";
});

// lastError 由轮询刷新：用户什么都没做，拒绝原因也会自己出现或消失。
const lastError = computed(() => snapshot.value?.lastError.trim() ?? "");
const showError = computed(
  () =>
    lastError.value !== "" &&
    (phase.value === "rejected" || phase.value === "error" || phase.value === "degraded"),
);

const errorLabel = computed(() => {
  if (phase.value === "rejected") return "控制面返回";
  if (phase.value === "error") return "本机错误";
  return "最近错误";
});

const pubkey = computed(() => snapshot.value?.pubkey ?? "");

const serverHost = computed(() => (snapshot.value?.serverUrl ?? "").replace(/^https?:\/\//, ""));

const meta = computed(() => {
  const current = snapshot.value;
  if (!current) return "";
  const parts: string[] = [];
  if (phase.value === "online") parts.push("最近心跳 " + clockTime(current.lastHeartbeat));
  if (serverHost.value) parts.push(serverHost.value);
  return parts.join(" · ");
});

// 公钥不是秘密：抄进控制台、发给自己都行，所以复制失败也只需要提示手动选中。
async function onCopyPubkey(): Promise<void> {
  if (!pubkey.value) return;
  copyError.value = "";
  try {
    await navigator.clipboard.writeText(pubkey.value);
    copied.value = true;
    window.setTimeout(() => (copied.value = false), 1500);
  } catch {
    copyError.value = "复制失败：浏览器拒绝了剪贴板访问，请手动选中公钥。";
  }
}

// 开机自启是系统级设置，勾一下就落盘，不等「保存并连接」。
const autostartOn = ref(false);
const autostartBusy = ref(false);
const autostartError = ref("");

watch(
  snapshot,
  (current) => {
    if (current) autostartOn.value = current.autostartEnabled;
  },
  { immediate: true },
);

async function onAutostart(event: Event): Promise<void> {
  const box = event.target as HTMLInputElement;
  autostartBusy.value = true;
  autostartError.value = "";
  try {
    const next = await setAutostart(box.checked);
    autostartOn.value = next.autostartEnabled;
  } catch (err) {
    // 没设置成功就把复选框拨回原样，别让界面显示一个不存在的状态。
    box.checked = autostartOn.value;
    autostartError.value = "设置开机自启失败：" + errorMessage(err);
  } finally {
    autostartBusy.value = false;
  }
}

// 保存成功只代表本地配置写好了：最多挂几秒说明「正在连接」，之后由状态区接着讲。
let okTimer = 0;

function note(text: string, kind: "" | "ok" | "error"): void {
  window.clearTimeout(okTimer);
  message.kind = kind;
  message.text = text;
  if (kind === "ok") {
    okTimer = window.setTimeout(() => {
      message.kind = "";
      message.text = "";
    }, 5000);
  }
}

onUnmounted(() => window.clearTimeout(okTimer));

async function onSave(): Promise<void> {
  saving.value = true;
  note("", "");
  try {
    const next = await saveSettings({
      serverUrl: form.serverUrl,
      hostname: form.hostname,
    });
    fill({ serverUrl: next.serverUrl, hostname: next.hostnameSet });
    note("已保存，正在连接…", "ok");
  } catch (err) {
    // 只有本机写盘失败才走到这里；控制面拒绝会出现在上面状态区的 lastError 里。
    note("保存失败：" + errorMessage(err), "error");
  } finally {
    saving.value = false;
  }
}

// 删除本地配置可逆的部分只有「重新填一次地址」：公私钥与服务端槽位都不动。
const resetting = ref(false);
const resetNote = reactive({ text: "", kind: "" as "" | "ok" | "error" });

async function onReset(): Promise<void> {
  const ok = window.confirm(
    "删除本地配置？\n\n" +
      "会断开连接，并删除本机的 client.json（控制面地址与设备名）、config/config.toml（下发的网络配置）与 config/credential.json（控制面签发的凭据），回到首次配置状态。\n\n" +
      "不会删除本机的公私钥（公钥不变），也不会动控制台上的设备槽位。删完重新填地址保存即可再连。",
  );
  if (!ok) return;
  resetting.value = true;
  resetNote.kind = "";
  resetNote.text = "";
  try {
    await resetConfig();
    note("", "");
    // 删完就回到首次配置的输入状态：地址回到默认值，设备名清空。
    filled = false;
    fill({ serverUrl: "", hostname: "" });
    resetNote.kind = "ok";
    resetNote.text = "本地配置已删除。";
  } catch (err) {
    resetNote.kind = "error";
    resetNote.text = "删除失败：" + errorMessage(err);
  } finally {
    resetting.value = false;
  }
}
</script>

<template>
  <div class="view">
    <div class="page">
      <!-- 状态区是这一页的第一结论：连没连上、为什么没连上，都在这里。 -->
      <section class="state" :class="phase">
        <div class="state-head">
          <i class="dot" aria-hidden="true" />
          <h1>{{ title }}</h1>
          <span v-if="meta" class="state-meta" :title="snapshot?.serverUrl || ''">{{ meta }}</span>
        </div>
        <p class="state-detail">{{ detail }}</p>

        <div v-if="showError" class="error-box">
          <span class="error-label">{{ errorLabel }}</span>
          <code class="error-text">{{ lastError }}</code>
          <p v-if="phase === 'rejected'" class="error-hint">
            401 表示控制面不认这把公钥：把下面这把公钥填进控制面的「新建设备」；它已经绑在别的槽位上时，用「换钥匙」改成这一把。
          </p>
          <p v-else-if="phase === 'error'" class="error-hint">这是本机的问题，与公钥无关；修好之后下一轮轮询会自己恢复。</p>
        </div>

        <div class="pubkey" :class="{ fix: phase === 'rejected' }">
          <div class="pubkey-head">
            <span class="pubkey-label">本机公钥</span>
            <button type="button" class="copy" :disabled="!pubkey" @click="onCopyPubkey">
              {{ copied ? "已复制" : "复制" }}
            </button>
          </div>
          <code class="pubkey-value" :title="pubkey">{{ pubkey || "暂未生成" }}</code>
          <p v-if="pubkey" class="hint">
            控制面「新建设备」里的公钥就是这一串。它由本机私钥推出，删配置、换版本都不变。
          </p>
          <p v-else class="hint">客户端还没生成密钥，等状态刷出来再复制。</p>
          <p v-if="copyError" class="copy-error">{{ copyError }}</p>
        </div>
      </section>

      <form class="form" @submit.prevent="onSave">
        <section class="card">
          <header>
            <h2>连接</h2>
            <p>保存只写本地配置并开始连接；连没连上由上面的状态区回答。</p>
          </header>
          <div class="field">
            <label for="serverUrl">控制面地址</label>
            <input
              id="serverUrl"
              v-model="form.serverUrl"
              type="text"
              placeholder="http://10.126.126.1:8088"
              autocomplete="off"
              spellcheck="false"
            />
            <span class="hint">管理页所在地址，带 http:// 或 https://。</span>
          </div>
          <div class="field">
            <label for="hostname">设备名</label>
            <input id="hostname" v-model="form.hostname" type="text" placeholder="留空使用本机主机名" autocomplete="off" />
            <span class="hint">
              写进 config.toml 的 hostname：出现在控制台的设备列表和其他节点的节点列表里；留空时用本机主机名。
            </span>
          </div>

          <footer class="card-foot">
            <span class="msg" :class="message.kind" role="status">{{ message.text }}</span>
            <button type="submit" class="primary" :disabled="saving">{{ saving ? "保存中…" : "保存并连接" }}</button>
          </footer>
        </section>
      </form>

      <section class="card">
        <header>
          <h2>开机自启</h2>
          <p>系统级设置，勾选即生效，不需要保存。</p>
        </header>
        <template v-if="snapshot?.autostartSupported">
          <label class="check">
            <input type="checkbox" :checked="autostartOn" :disabled="autostartBusy" @change="onAutostart" />
            <span>登录后自动启动客户端</span>
          </label>
          <span class="hint">
            在任务计划程序里建一条「Mesh Client」任务：登录触发、最高权限运行，开机不弹 UAC；托盘菜单里也能切换。
          </span>
          <p v-if="autostartError" class="field-error">{{ autostartError }}</p>
        </template>
        <p v-else class="hint">当前平台不支持开机自启。</p>
        <p class="core-note" :title="snapshot?.corePathUsed || ''">
          core 取自安装根的 bin/，当前使用 {{ snapshot?.corePathUsed || "—" }}
        </p>
      </section>

      <!-- 破坏性操作隔一条线再往下放，和「保存」保持足够距离，不并排成等权重卡片。 -->
      <div class="divider" aria-hidden="true" />

      <section class="card danger">
        <header>
          <h2>删除本地配置</h2>
          <p>断开连接并删掉本机的配置，回到首次配置状态。公钥与控制台上的槽位都不变。</p>
        </header>
        <dl class="scope">
          <div>
            <dt>会删除</dt>
            <dd>client.json（控制面地址与设备名）、config/config.toml（下发的网络配置）、config/credential.json（控制面签发的凭据）</dd>
          </div>
          <div>
            <dt>不会删</dt>
            <dd>本机公私钥（公钥不变）、控制台上的设备槽位与日志。控制面上的设备记录要单独删。</dd>
          </div>
        </dl>
        <div class="danger-foot">
          <button type="button" class="danger-btn" :disabled="resetting" @click="onReset">
            {{ resetting ? "删除中…" : "删除本地配置" }}
          </button>
          <span class="msg" :class="resetNote.kind">{{ resetNote.text }}</span>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
/* 滚动条属于整页，贴在窗口最右侧；限宽只作用于内容，别让滚动条跑进居中的那一列。
   滚动条槽位常驻，内容变长变短时卡片宽度不跟着抖。 */
.view{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;overflow-y:auto;scrollbar-gutter:stable}

/* 单列居中：状态区最上，配置居中，破坏性操作单独一段。 */
.page{width:100%;max-width:640px;margin:0 auto;padding:24px 22px 44px;display:flex;flex-direction:column;gap:12px}
.form{display:flex;flex-direction:column;gap:12px}

/* 状态区用左边一条色条给结论，其余与卡片一致：不给它加底纹和阴影。 */
.state{border:1px solid var(--line);border-left:3px solid var(--faint);border-radius:9px;background:var(--card);padding:15px 18px 16px}
.state.online{border-left-color:var(--green)}
.state.connecting{border-left-color:var(--blue)}
.state.rejected,.state.error{border-left-color:var(--red)}
.state.degraded{border-left-color:var(--orange)}

.state-head{display:flex;align-items:center;gap:8px;flex-wrap:wrap;min-width:0}
.state-head h1{margin:0;font-size:15px;line-height:1.35;font-weight:500}
.state.online h1{color:var(--green)}
.state.connecting h1{color:var(--blue)}
.state.rejected h1,.state.error h1{color:var(--red)}
.state.degraded h1{color:var(--orange)}
.dot{flex:0 0 auto;width:7px;height:7px;border-radius:50%;background:var(--faint)}
.state.online .dot{background:var(--green)}
.state.connecting .dot{background:var(--blue);animation:pulse 1.4s infinite}
.state.rejected .dot,.state.error .dot{background:var(--red)}
.state.degraded .dot{background:var(--orange)}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.3}}

/* 心跳与地址是状态的补充；窗口窄时让位给标题，别把标题挤成两行。 */
.state-meta{min-width:0;margin-left:auto;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:11.5px;color:var(--muted);font-variant-numeric:tabular-nums}
.state-detail{margin:6px 0 0;font-size:12.5px;line-height:1.6;color:var(--muted)}

/* 拒绝原因原样贴出来：这是用户唯一能看到 401 的地方，不能截断。 */
.error-box{margin-top:11px;padding:9px 11px;border:1px solid #f3cfcf;border-radius:7px;background:#fdf6f6}
.error-label{display:block;margin-bottom:3px;font-size:10.5px;letter-spacing:.04em;color:var(--red)}
.error-text{display:block;font-family:var(--mono);font-size:11.5px;line-height:1.6;color:var(--red);white-space:pre-wrap;word-break:break-word;user-select:all}
.error-hint{margin:6px 0 0;font-size:11px;line-height:1.6;color:var(--muted)}

/* 公钥块：等宽、可整段选中；长串按字符换行，不撑破卡片。 */
.pubkey{margin-top:12px;padding:9px 11px;border:1px solid var(--line);border-radius:7px;background:var(--wash)}
.pubkey.fix{border-color:#c7d7fb;background:#f7faff}
.pubkey-head{display:flex;align-items:center;gap:10px}
.pubkey-label{font-size:11px;color:var(--muted)}
.pubkey-head .copy{margin-left:auto}
.pubkey-value{display:block;margin-top:5px;font-family:var(--mono);font-size:11.5px;line-height:1.6;color:var(--ink);word-break:break-all;user-select:all}
.copy-error{margin:5px 0 0;font-size:11px;color:var(--red)}

.card{border:1px solid var(--line);border-radius:9px;background:var(--card);padding:15px 18px 16px}
.card header{margin-bottom:12px}
.card h2{margin:0;font-size:13px;font-weight:500}
.card header p{margin:3px 0 0;font-size:11.5px;line-height:1.6;color:var(--muted)}

.field{display:grid;gap:5px}
.field+.field{margin-top:12px}
.field label{font-size:12px;color:var(--muted)}

.hint{display:block;margin:5px 0 0;font-size:11px;line-height:1.6;color:var(--faint)}
.field .hint{margin-top:0}
.field-error{margin:6px 0 0;font-size:11.5px;color:var(--red)}

input[type=text]{width:100%;padding:7px 10px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12.5px;outline:none}
input[type=text]:focus{border-color:var(--blue)}
input::placeholder{color:var(--faint)}

/* 复选框不套用上面那套输入框尺寸，否则会被拉成整行宽、边框和背景也乱。 */
.check{display:flex;align-items:center;gap:8px;font-size:12.5px;cursor:pointer}
.check input{width:auto;margin:0;accent-color:var(--blue)}

.core-note{margin:12px 0 0;padding-top:10px;border-top:1px solid var(--line-soft);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:10.5px;color:var(--faint)}

/* 保存属于「连接」这张卡：按钮和反馈贴在卡片底部，和下面的系统设置分开。 */
.card-foot{display:flex;align-items:center;gap:12px;margin-top:14px;padding-top:12px;border-top:1px solid var(--line-soft)}
.msg{min-width:0;overflow-wrap:anywhere;font-size:12px;color:var(--muted)}
.msg.ok{color:var(--green)}
.msg.error{color:var(--red)}
.card-foot button{margin-left:auto}

button{padding:5px 12px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12px;cursor:pointer}
button.primary{padding:6px 16px;border-color:var(--blue);background:var(--blue);color:#fff}
button:hover:not(:disabled){background:var(--wash)}
button.primary:hover:not(:disabled){background:#1d4ed8}
button:disabled{opacity:.45;cursor:default}

/* 破坏性操作单独一片区域：分隔线留白 + 红边，与「保存」拉开距离。 */
.divider{margin-top:20px;border-top:1px solid var(--line)}
.card.danger{border-color:#f0d5d5}
.card.danger h2{color:var(--red)}
.scope{display:grid;gap:7px;margin:0}
.scope>div{display:grid;grid-template-columns:56px minmax(0,1fr);gap:10px;font-size:11.5px;line-height:1.6}
.scope dt{color:var(--faint)}
.scope dd{margin:0;color:var(--muted)}
.danger-foot{display:flex;align-items:center;gap:12px;margin-top:12px}
button.danger-btn{border-color:#eec9c9;color:var(--red)}
button.danger-btn:hover:not(:disabled){background:#fdf4f4}

@media(max-width:520px){
  .page{padding:16px 14px 32px}
  .state-meta{margin-left:0;width:100%}
}
</style>
