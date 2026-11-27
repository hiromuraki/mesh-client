<script setup lang="ts">
import { reactive, ref, watch } from "vue";
import { resetConfig, saveSettings, setAutostart, useClient } from "../client";
import { errorMessage } from "../format";

const { snapshot } = useClient();

const DEFAULT_SERVER_URL = "http://127.0.0.1:8088";

const form = reactive({ serverUrl: "", token: "", hostname: "" });
const message = reactive({ text: "", kind: "" as "" | "ok" | "error" });
const saving = ref(false);

// 表单只回填一次：之后由用户输入驱动，轮询不会把正在输入的内容覆盖掉。
let filled = false;

function fill(next: { serverUrl: string; hostname: string } | null): void {
  if (!next) return;
  form.serverUrl = next.serverUrl || DEFAULT_SERVER_URL;
  form.hostname = next.hostname;
  form.token = "";
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

// 开机自启是系统级设置，勾一下就落盘，不等「保存并连接」：
// 它跟服务端地址、设备名这些要重启 core 的字段不是一回事。
const autostartOn = ref(false);
const autostartBusy = ref(false);

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
  message.kind = "";
  message.text = "";
  try {
    const next = await setAutostart(box.checked);
    autostartOn.value = next.autostartEnabled;
  } catch (err) {
    // 设置没成功就把复选框拨回原样，别让界面显示一个不存在的状态。
    box.checked = autostartOn.value;
    message.kind = "error";
    message.text = "设置开机自启失败: " + errorMessage(err);
  } finally {
    autostartBusy.value = false;
  }
}

// 删除本地配置是不可逆的：删掉下发的 config.toml 与设备身份，要重新用 token 注册。
// 放在设置页最底部，和数据页的日常操作分开。
async function onReset(): Promise<void> {
  const ok = window.confirm(
    "删除本地配置？\n\n会断开连接、删除下发的 config.toml，并清空设备身份；需要重新用 token 注册。\n服务端上的设备记录不会被删除。",
  );
  if (!ok) return;
  message.kind = "";
  message.text = "";
  try {
    await resetConfig();
  } catch (err) {
    message.kind = "error";
    message.text = "删除本地配置失败: " + errorMessage(err);
  }
}

async function onSave(): Promise<void> {
  saving.value = true;
  message.kind = "";
  message.text = "保存中…";
  try {
    const next = await saveSettings({
      serverUrl: form.serverUrl,
      token: form.token,
      hostname: form.hostname,
    });
    fill({ serverUrl: next.serverUrl, hostname: next.hostnameSet });
    message.kind = "ok";
    message.text = next.deviceId ? "已保存，正在连接…" : "已保存";
  } catch (err) {
    message.kind = "error";
    message.text = errorMessage(err);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="view">
    <form class="stack" @submit.prevent="onSave">
      <section class="card">
        <header>
          <h2>连接</h2>
          <p>保存后 core 会重启并重新注册，约 2 秒生效。</p>
        </header>
        <div class="field">
          <label for="serverUrl">服务端地址</label>
          <input id="serverUrl" v-model="form.serverUrl" type="text" placeholder="http://10.8.0.1:8088" autocomplete="off" spellcheck="false" />
          <span class="note">管理页所在地址</span>
        </div>
        <div class="field">
          <label for="token">注册令牌</label>
          <input id="token" v-model="form.token" type="password" placeholder="粘贴管理页生成的 token" autocomplete="off" />
          <span class="note">首次注册用，注册成功后即可留空</span>
        </div>
      </section>

      <section class="card">
        <header>
          <h2>客户端</h2>
          <p>设备名会出现在其他节点的列表里。</p>
        </header>
        <div class="field">
          <label for="hostname">设备名</label>
          <input id="hostname" v-model="form.hostname" type="text" placeholder="留空使用本机主机名" autocomplete="off" />
        </div>
        <p class="note" :title="snapshot?.corePathUsed || ''">core 取自安装根的 bin/，当前使用 {{ snapshot?.corePathUsed || "—" }}</p>
        <div v-if="snapshot?.autostartSupported" class="field">
          <label class="check">
            <input type="checkbox" :checked="autostartOn" :disabled="autostartBusy" @change="onAutostart" />
            <span>登录后自动启动客户端</span>
          </label>
          <span class="note">写入任务计划程序，以最高权限运行，开机不弹 UAC；托盘菜单里也能切换</span>
        </div>
      </section>

      <div class="submit">
        <span class="msg" :class="message.kind">{{ message.text }}</span>
        <button type="submit" class="primary" :disabled="saving">保存并连接</button>
      </div>

      <section class="card danger">
        <header>
          <h2>删除本地配置</h2>
          <p>删掉下发的 config.toml 与设备身份，需要重新用 token 注册。服务端的设备记录不受影响。</p>
        </header>
        <div class="row">
          <span class="mono">{{ snapshot?.deviceId || "尚未注册" }}</span>
          <button type="button" class="danger-btn" @click="onReset">删除本地配置</button>
        </div>
      </section>
    </form>
  </div>
</template>

<style scoped>
/* 滚动条属于整页，贴在窗口最右侧；限宽只作用于内容，别让滚动条跑进居中的那一列。
   滚动条槽位常驻，内容变长变短时卡片宽度不跟着抖。 */
.view{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;overflow-y:auto;scrollbar-gutter:stable}

/* 单列居中：卡片宽度固定，字段不再横着摊满整屏。 */
.stack{width:100%;max-width:600px;margin:0 auto;padding:26px 24px 40px;display:flex;flex-direction:column;gap:14px}
.card{border:1px solid var(--line);border-radius:10px;background:var(--card);padding:16px 18px 18px}
.card header{margin-bottom:14px}
.card h2{margin:0;font-size:13px;font-weight:500}
.card header p{margin:3px 0 0;font-size:11.5px;color:var(--muted)}

.field{display:grid;gap:5px}
.field+.field{margin-top:12px}
.field label{font-size:12px;color:var(--muted)}
.field .note{font-size:11px;color:var(--faint);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}

input{width:100%;padding:7px 10px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12.5px;outline:none}
input:focus{border-color:var(--blue)}
input::placeholder{color:var(--faint)}

/* 复选框不套用上面那套输入框尺寸，否则会被拉成整行宽、边框和背景也乱。 */
.field .check{display:flex;align-items:center;gap:7px;font-size:12.5px;cursor:pointer}
.field .check input{width:auto;margin:0;padding:0;border:none;background:none;accent-color:var(--blue)}

.submit{display:flex;align-items:center;gap:12px;padding:2px 2px 0}
.msg{font-size:12px;color:var(--muted)}
.msg.error{color:var(--red)}
.msg.ok{color:var(--green)}
.submit button{margin-left:auto}

button{padding:6px 16px;border:1px solid var(--line);border-radius:6px;background:var(--card);color:var(--ink);font:inherit;font-size:12.5px;cursor:pointer}
button.primary{border-color:var(--blue);background:var(--blue);color:#fff}
button:hover:not(:disabled){background:var(--wash)}
button.primary:hover:not(:disabled){background:#1d4ed8}
button:disabled{opacity:.5;cursor:default}

/* 破坏性操作单独一张卡，用红边和正文区隔开，避免和"保存"混在一起点。 */
.card.danger{margin-top:6px;border-color:#f0d5d5}
.card.danger h2{color:var(--red)}
.row{display:flex;align-items:center;gap:12px}
.row .mono{margin-right:auto;font-size:11.5px;color:var(--faint);font-variant-numeric:tabular-nums}
button.danger-btn{border-color:#eec9c9;color:var(--red)}
button.danger-btn:hover:not(:disabled){background:#fdf4f4}
</style>