<script setup lang="ts">
import { ref, onMounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { RefreshCw, Search, Copy, X } from "lucide-vue-next";
import { toast, friendlyError } from "../composables/useToast";
import SelectMenu from "../components/SelectMenu.vue";
import ConfirmDialog from "../components/ConfirmDialog.vue";

interface ConfigDto {
  phpstudy_path: string; www_root: string; active_web_server: string;
  auto_start: string[]; mysql_port: number; redis_port: number;
  log_dir: string; log_retention_days: number;
  stop_services_on_exit: boolean;
  phpstudy_compatible_workers: boolean;
  php_worker_budget: number; php_workers_per_version: number; php_max_requests: number;
}

const config = ref<ConfigDto>({ phpstudy_path: "", www_root: "", active_web_server: "nginx", auto_start: [], mysql_port: 3306, redis_port: 6379, log_dir: "", log_retention_days: 7, stop_services_on_exit: false, phpstudy_compatible_workers: true, php_worker_budget: 8, php_workers_per_version: 4, php_max_requests: 1000 });
const busy = ref(false);
const saved = ref(false);
const logRetentionOptions = [
  { label: "3 天", value: 3 },
  { label: "7 天", value: 7 },
  { label: "30 天", value: 30 },
  { label: "90 天", value: 90 },
];

// Theme
const themeMode = ref(localStorage.getItem("naxone-theme") || "dark");
function setTheme(mode: string) {
  themeMode.value = mode;
  localStorage.setItem("naxone-theme", mode);
  window.dispatchEvent(new CustomEvent("theme-change", { detail: mode }));
}

function showError(msg: unknown) { toast.error(friendlyError(msg)); }

async function loadConfig() { try { config.value = await invoke("get_config"); } catch (e) { showError("加载配置失败: " + e); } }

async function saveSettings() {
  busy.value = true;
  try {
    const ps = (config.value.phpstudy_path || "").replace(/\\/g, "/").replace(/\/+$/, "");
    if (ps && !config.value.www_root) {
      config.value.www_root = ps + "/WWW";
    }
    await invoke("save_config", { dto: config.value });
    await invoke("rescan_services");
    saved.value = true; setTimeout(() => (saved.value = false), 2000);
  } catch (e) { showError("保存失败: " + e); } finally { busy.value = false; }
}

async function openLogDir() {
  try { await invoke("open_log_dir"); } catch (e) { showError("打开失败: " + e); }
}

async function rescan() {
  busy.value = true;
  try { await invoke("rescan_services"); saved.value = true; setTimeout(() => (saved.value = false), 2000); }
  catch (e) { showError("扫描失败: " + e); } finally { busy.value = false; }
}

function toggleAutoStart(service: string) {
  const idx = config.value.auto_start.indexOf(service);
  if (idx >= 0) config.value.auto_start.splice(idx, 1);
  else config.value.auto_start.push(service);
}

// ─── 端口检测 ───────────────────────────────────
interface PortListener {
  pid: number;
  process_name: string | null;
  exe_path: string | null;
  source: string | null;
  local_address: string;
}
interface PortDiagnosis {
  port: number;
  in_use: boolean;
  listeners: PortListener[];
}

const testPort = ref<number>(80);
const diagResult = ref<PortDiagnosis | null>(null);
const diagBusy = ref(false);
const showKillConfirm = ref(false);
const killBusy = ref(false);
const killTarget = ref<PortListener | null>(null);

async function runDiagnose() {
  if (!testPort.value || testPort.value < 1 || testPort.value > 65535) {
    showError("端口号必须在 1–65535 之间");
    return;
  }
  diagBusy.value = true;
  try {
    diagResult.value = await invoke<PortDiagnosis>("diagnose_port", { port: testPort.value });
  } catch (e) {
    showError("检测失败: " + e);
  } finally {
    diagBusy.value = false;
  }
}

async function copyPid(pid: number) {
  try {
    await navigator.clipboard.writeText(String(pid));
    toast.success(`已复制 PID ${pid}`);
  } catch {
    showError("复制失败");
  }
}

function askKill(l: PortListener) {
  killTarget.value = l;
  showKillConfirm.value = true;
}

async function confirmKill() {
  if (!killTarget.value) return;
  killBusy.value = true;
  try {
    await invoke("kill_process_by_pid", { pid: killTarget.value.pid });
    toast.success(`已结束进程 PID ${killTarget.value.pid}`);
    showKillConfirm.value = false;
    killTarget.value = null;
    await runDiagnose();
  } catch (e) {
    showError("结束进程失败: " + e);
  } finally {
    killBusy.value = false;
  }
}

// ─── 应用更新 ───────────────────────────────────
const currentVersion = ref("");
const latestVersion = ref("");
const updateAvailable = ref(false);
const checking = ref(false);
const checked = ref(false);
const updating = ref(false);
const lastCheckedAt = ref("");

async function loadCurrentVersion() {
  try {
    const { getVersion } = await import("@tauri-apps/api/app");
    currentVersion.value = await getVersion();
  } catch { /* 旧版本可能 ACL 拒绝，忽略 */ }
}

async function checkUpdate() {
  if (checking.value || updating.value) return;
  checking.value = true;
  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const upd = await check();
    if (upd) {
      updateAvailable.value = true;
      latestVersion.value = upd.version;
    } else {
      updateAvailable.value = false;
      latestVersion.value = "";
    }
    checked.value = true;
    lastCheckedAt.value = new Date().toLocaleString();
  } catch (e) {
    showError("检查更新失败: " + e);
  } finally {
    checking.value = false;
  }
}

async function doUpdate() {
  if (updating.value) return;
  updating.value = true;
  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const upd = await check();
    if (!upd) { toast.info("已是最新版本"); updateAvailable.value = false; return; }
    await upd.downloadAndInstall();
    const { relaunch } = await import("@tauri-apps/plugin-process");
    await relaunch();
  } catch (e) {
    showError("更新失败: " + e);
  } finally {
    updating.value = false;
  }
}

onMounted(() => { loadConfig(); loadCurrentVersion(); });
</script>

<template>
  <div class="max-w-[640px] has-save-bar">

    <!-- Theme -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">外观主题</h2>
      <div class="flex gap-3">
        <label
          v-for="t in [{v:'dark',icon:'☾',l:'暗色'},{v:'light',icon:'☀',l:'亮色'},{v:'auto',icon:'◐',l:'跟随系统'}]"
          :key="t.v"
          class="flex-1 flex items-center gap-2.5 px-4 py-3 border border-border rounded-lg cursor-pointer transition-all"
          :class="themeMode === t.v ? 'border-accent-blue bg-[rgba(29,78,216,0.1)]' : ''"
          @click="setTheme(t.v)"
        >
          <input type="radio" v-model="themeMode" :value="t.v" class="hidden" />
          <span class="text-lg">{{ t.icon }}</span>
          <span class="text-[16px]">{{ t.l }}</span>
        </label>
      </div>
    </div>

    <!-- General -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">基本设置</h2>
      <div class="grid grid-cols-1 gap-4">
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">PHPStudy 安装路径</label>
          <input class="input" v-model="config.phpstudy_path" placeholder="D:\phpstudy_pro" />
          <p class="text-[13px] text-content-muted mt-1">扫描此目录下的 Extensions 发现已安装的服务；默认站点目录使用 NaxOne 自己管理的 www 路径，不跟随 PHPStudy 的 WWW</p>
        </div>
      </div>
    </div>

    <!-- Web Server -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">Web 服务器</h2>
      <div class="flex gap-3">
        <label
          v-for="ws in [{v:'nginx',c:'#009639',i:'N'},{v:'apache',c:'#d22128',i:'A'}]"
          :key="ws.v"
          class="flex-1 flex items-center gap-2.5 px-4 py-3 border border-border rounded-lg cursor-pointer transition-all"
          :class="config.active_web_server === ws.v ? 'border-accent-blue bg-[rgba(29,78,216,0.1)]' : ''"
        >
          <input type="radio" v-model="config.active_web_server" :value="ws.v" class="hidden" />
          <span class="w-6 h-6 rounded flex items-center justify-center text-[13px] font-bold text-white" :style="{ background: ws.c }">{{ ws.i }}</span>
          <span class="text-[16px] capitalize">{{ ws.v }}</span>
        </label>
      </div>
      <p class="text-[13px] text-content-muted mt-2.5">"全部启动" 时只启动选中的 Web 服务器</p>
    </div>

    <!-- Ports -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">端口配置</h2>
      <div class="grid grid-cols-2 gap-4">
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">MySQL 端口</label>
          <input class="input" type="number" v-model.number="config.mysql_port" min="1" max="65535" />
        </div>
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">Redis 端口</label>
          <input class="input" type="number" v-model.number="config.redis_port" min="1" max="65535" />
        </div>
      </div>
    </div>

    <!-- PHP FastCGI -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">PHP FastCGI 并发</h2>
      <label class="flex items-center gap-2 text-[16px] cursor-pointer mb-4">
        <input type="checkbox" v-model="config.phpstudy_compatible_workers" class="accent-accent-success w-4 h-4" />
        <span>PHPStudy 兼容模式（每个活跃 PHP 版本固定 16 workers）</span>
      </label>
      <div class="grid grid-cols-3 gap-4">
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">Worker 总预算</label>
          <input class="input" type="number" v-model.number="config.php_worker_budget" min="1" max="64" :disabled="config.phpstudy_compatible_workers" />
        </div>
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">单版本上限</label>
          <input class="input" type="number" v-model.number="config.php_workers_per_version" min="1" max="16" :disabled="config.phpstudy_compatible_workers" />
        </div>
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">最大请求数</label>
          <input class="input" type="number" v-model.number="config.php_max_requests" min="100" max="10000" step="100" />
        </div>
      </div>
      <p class="text-[13px] text-content-muted mt-2.5">
        仍然只启动启用站点实际使用的 PHP。关闭兼容模式后，才使用总预算和单版本上限进行加权分配。
      </p>
    </div>

    <!-- Port Diagnosis -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">端口检测</h2>
      <p class="text-[13px] text-content-muted mb-3">输入端口号查看占用进程，可一键结束。</p>
      <div class="flex items-center gap-2 mb-3">
        <input class="input" type="number" min="1" max="65535" v-model.number="testPort"
               placeholder="端口号" style="max-width: 160px"
               @keyup.enter="runDiagnose" />
        <button class="btn btn-primary btn-sm flex items-center gap-1" :disabled="diagBusy" @click="runDiagnose">
          <Search :size="14" />
          {{ diagBusy ? '检测中…' : '检测' }}
        </button>
      </div>

      <div v-if="diagResult">
        <!-- 空闲 -->
        <div v-if="!diagResult.in_use" class="text-[13px]"
             style="color: var(--color-success); padding: 10px 12px; background: var(--bg-tertiary); border: 1px solid var(--border-color); border-radius: 8px">
          ✅ 端口 {{ diagResult.port }} 空闲
        </div>

        <!-- 占用 -->
        <div v-else>
          <div class="text-[13px] mb-2" style="color: var(--color-danger)">
            ⚠️ 端口 {{ diagResult.port }} 被占用（{{ diagResult.listeners.length }} 个监听）
          </div>
          <div v-for="l in diagResult.listeners" :key="l.pid"
               style="padding: 10px 12px; background: var(--bg-tertiary); border: 1px solid var(--border-color); border-radius: 8px; margin-bottom: 8px">
            <div class="text-[13px]" style="color: var(--text-primary)">
              <span style="font-family: var(--font-mono)">{{ l.process_name || '未知进程' }}</span>
              <span style="color: var(--text-muted)"> · PID {{ l.pid }}</span>
              <span v-if="l.source" class="ml-2 px-1.5 py-px rounded text-[12px]"
                    style="background: var(--color-blue); color: white">{{ l.source }}</span>
            </div>
            <div v-if="l.exe_path" class="text-[12px] mt-1" style="color: var(--text-muted); word-break: break-all">
              {{ l.exe_path }}
            </div>
            <div class="text-[12px] mt-1" style="color: var(--text-muted)">
              监听：{{ l.local_address }}
            </div>
            <div class="flex gap-2 mt-2">
              <button class="btn btn-secondary btn-sm flex items-center gap-1" @click="copyPid(l.pid)">
                <Copy :size="12" /> 复制 PID
              </button>
              <button class="btn btn-danger btn-sm flex items-center gap-1" @click="askKill(l)">
                <X :size="12" /> 结束进程
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- Auto Start -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">自动启动</h2>
      <p class="text-[13px] text-content-muted mb-3">选择应用启动时自动启动的服务</p>
      <div class="flex flex-wrap gap-3">
        <label v-for="svc in ['nginx','apache','mysql','redis']" :key="svc" class="flex items-center gap-2 text-[16px] cursor-pointer">
          <input type="checkbox" :checked="config.auto_start.includes(svc)" @change="toggleAutoStart(svc)" class="accent-accent-success w-4 h-4" />
          <span class="capitalize">{{ svc }}</span>
        </label>
      </div>
    </div>

    <!-- Exit behavior -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">退出行为</h2>
      <label class="flex items-center gap-2 text-[16px] cursor-pointer mb-2">
        <input type="checkbox" v-model="config.stop_services_on_exit" class="accent-accent-success w-4 h-4" />
        <span>退出应用时自动停止所有服务</span>
      </label>
      <p class="text-[13px] text-content-muted">仅托盘菜单“退出”生效；点窗口右上角关闭只是最小化到托盘。</p>
    </div>

    <!-- Log Settings -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">日志设置</h2>
      <div class="grid grid-cols-2 gap-4">
        <div class="flex flex-col gap-1.5 col-span-2">
          <label class="text-[16px] text-content-secondary font-medium">日志目录 <span class="text-[13px] text-content-muted font-normal">留空使用默认（exe 同级 logs/）</span></label>
          <div class="flex gap-2">
            <input class="input flex-1" v-model="config.log_dir" placeholder="默认位置" />
            <button class="btn btn-secondary btn-sm" @click="openLogDir">打开目录</button>
          </div>
        </div>
        <div class="flex flex-col gap-1.5">
          <label class="text-[16px] text-content-secondary font-medium">保留天数</label>
          <SelectMenu v-model="config.log_retention_days" :options="logRetentionOptions" full-width trigger-class="input" />
        </div>
      </div>
    </div>

    <!-- 应用更新 -->
    <div class="card mb-3">
      <h2 class="text-[16px] font-medium text-content-secondary mb-3">应用更新</h2>
      <div class="flex items-center gap-3">
        <!-- 左：当前版本（label + 数字横排，baseline 对齐） -->
        <div class="flex items-baseline gap-2">
          <span class="text-[13px] text-content-muted">当前版本</span>
          <span class="text-[16px] font-medium tabular-nums">v{{ currentVersion || "—" }}</span>
        </div>

        <!-- 圆形刷新按钮 -->
        <button
          class="relative w-8 h-8 rounded-full flex items-center justify-center transition-transform hover:scale-105 active:scale-95 disabled:opacity-60 disabled:cursor-wait disabled:hover:scale-100 disabled:active:scale-100"
          :disabled="checking || updating"
          @click="checkUpdate"
          :title="checking ? '检查中...' : '检查更新'"
          style="background: rgba(99,128,255,0.12); border: 1px solid var(--border-color)"
        >
          <RefreshCw :size="14" :class="checking ? 'animate-spin' : ''" :style="{ color: 'var(--accent-blue)' }" />
        </button>

        <!-- 右：状态（推到最右） -->
        <div class="ml-auto flex items-center gap-2">
          <template v-if="updating">
            <span class="text-[13px] text-content-muted">更新中...</span>
          </template>
          <template v-else-if="updateAvailable">
            <span class="relative flex h-2.5 w-2.5 shrink-0">
              <span class="animate-ping absolute inline-flex h-full w-full rounded-full opacity-75" style="background:#ef4444"></span>
              <span class="relative inline-flex rounded-full h-2.5 w-2.5" style="background:#ef4444"></span>
            </span>
            <button class="text-[13px] hover:underline" @click="doUpdate" :style="{ color: 'var(--accent-blue)' }">
              有新版本 v{{ latestVersion }}，点击更新
            </button>
          </template>
          <template v-else-if="checked">
            <span class="text-[13px] text-content-muted">已是最新版本</span>
          </template>
          <template v-else>
            <span class="text-[13px] text-content-muted">点击按钮检查更新</span>
          </template>
        </div>
      </div>
      <p v-if="lastCheckedAt" class="text-[13px] text-content-muted mt-2.5">上次检查：{{ lastCheckedAt }}</p>
    </div>

    <div class="save-bar">
      <button class="btn btn-success btn-sm" @click="saveSettings" :disabled="busy">{{ busy ? "保存中..." : "保存设置" }}</button>
      <button class="btn btn-secondary btn-sm" @click="rescan" :disabled="busy">重新扫描服务</button>
      <span v-if="saved" class="saved-msg">已保存</span>
    </div>

    <ConfirmDialog
      :open="showKillConfirm"
      title="结束进程"
      variant="danger"
      confirm-text="确认结束"
      :busy="killBusy"
      @confirm="confirmKill"
      @cancel="showKillConfirm = false"
    >
      确认结束 <b>{{ killTarget?.process_name || '未知进程' }}</b>（PID {{ killTarget?.pid }}）？
      <br />
      该进程相关的服务可能会立即停止。
    </ConfirmDialog>
  </div>
</template>
