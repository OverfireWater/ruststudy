<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { AlertCircle, AlertTriangle, CheckCircle2, Info, Bug, ChevronRight, Store, Settings2, RefreshCw } from "lucide-vue-next";
import { useRouter } from "vue-router";
import { APP_NAME } from "../composables/useAppInfo";
import SelectMenu from "../components/SelectMenu.vue";
import LogDrawer from "../components/LogDrawer.vue";
import ConfirmDialog from "../components/ConfirmDialog.vue";

const router = useRouter();

interface ServiceInfo {
  id: string; kind: string; display_name: string; version: string;
  variant: string | null; port: number;
  status: { state: string; pid?: number; memory_mb?: number };
  install_path: string;
  origin: string; // "phpstudy" | "store" | "manual"
}

interface ServiceBatchProgress {
  operation: "start" | "stop";
  id: string;
  status: ServiceInfo["status"];
}

interface LogEntry {
  id: number; timestamp: string; level: string; category: string;
  message: string; details?: string;
}

const services = ref<ServiceInfo[]>([]);
const loaded = ref(false); // 首次 loadServices 返回后置 true，用于切换 skeleton
// 每类服务当前选中的版本 id（下拉切版本用）。key 是 kind，value 是 ServiceInfo.id
const selectedByKind = ref<Record<string, string>>({});

// 本应用自身资源统计
interface AppStats { pid: number; memory_mb: number | null; uptime_secs: number; is_dev: boolean; }
const appStats = ref<AppStats | null>(null);
// 前端本地秒表：后端每 5s 给一次 uptime_secs 基准值，本地每秒 +1 实现秒级刷新
const localUptime = ref(0);


// 在线更新检查
interface UpdateInfo { available: boolean; latest_version: string; current_version: string; release_url: string; release_notes: string; download_url: string | null; }
const updateInfo = ref<UpdateInfo | null>(null);
const updateDismissed = ref(false);

// 外部进程检测：占着我们关心端口但 exe 不在已扫安装目录里的进程（如系统装的 redis、第三方 nginx）
interface StrangerService { kind: string; port: number; pid: number; exe_path: string; label: string; }
const strangers = ref<StrangerService[]>([]);
const strangerDismissed = ref<Set<number>>(new Set()); // 按 PID 标记本会话内已忽略的
const strangerKilling = ref<Set<number>>(new Set());

async function scanStrangers() {
  try {
    const list = await invokeWithTimeout<StrangerService[]>("scan_running_strangers", undefined, 8000);
    // 过滤掉本会话已忽略的 PID
    strangers.value = list.filter((s) => !strangerDismissed.value.has(s.pid));
  } catch (e) {
    // 静默失败：扫描属于辅助功能，失败不打扰用户
    tracingWarn(`外部进程扫描失败: ${e}`);
  }
}

function tracingWarn(msg: string) { console.warn("[strangers]", msg); }

async function killStranger(s: StrangerService) {
  if (strangerKilling.value.has(s.pid)) return;
  strangerKilling.value.add(s.pid);
  try {
    await invokeWithTimeout("kill_stranger", { pid: s.pid }, 8000);
    toast.success(`已结束 ${s.kind}（PID ${s.pid}）`);
    strangers.value = strangers.value.filter((x) => x.pid !== s.pid);
    // 顺手刷一下服务状态，端口释放后我方实例可能能正常启动
    await loadServices(true);
  } catch (e) {
    showError(`结束失败: ${e}`);
  } finally {
    strangerKilling.value.delete(s.pid);
  }
}

function dismissStranger(s: StrangerService) {
  strangerDismissed.value.add(s.pid);
  strangers.value = strangers.value.filter((x) => x.pid !== s.pid);
}

// 格式化运行时长："2h 13m" / "45m 30s" / "1d 2h"
function fmtUptime(s: number): string {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  if (s < 86400) {
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    return `${h}h ${m}m`;
  }
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  return `${d}d ${h}h`;
}

// 全局 CLI PHP（只读摘要，编辑在 /env 页）
interface GlobalPhpInfo { version: string | null; bin_dir: string; path_registered: boolean; conflicts: string[]; }
const globalPhp = ref<GlobalPhpInfo>({ version: null, bin_dir: "", path_registered: false, conflicts: [] });

import { toast, friendlyError } from "../composables/useToast";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
const busyIds = ref<Set<string>>(new Set());
const recentLogs = ref<LogEntry[]>([]);
const logDrawerOpen = ref(false);
const batchBusy = ref(false);

let svcTimer: number | null = null;
let logTimer: number | null = null;
let uptimeTimer: number | null = null;
let unlistenServicesChanged: UnlistenFn | null = null;
let unlistenGlobalEnvChanged: UnlistenFn | null = null;
let unlistenBatchProgress: UnlistenFn | null = null;
let pauseUntil = 0;
const INVOKE_TIMEOUT_MS = 10000;
const bgErrorAt = new Map<string, number>();

async function invokeWithTimeout<T>(
  command: string,
  args?: Record<string, unknown>,
  timeoutMs = INVOKE_TIMEOUT_MS,
): Promise<T> {
  const timeout = new Promise<never>((_, reject) => {
    window.setTimeout(() => reject(new Error(`${command} 超时`)), timeoutMs);
  });
  return await Promise.race([invoke<T>(command, args), timeout]);
}

// ==================== Computed ====================

interface KindGroup {
  kind: string;
  all: ServiceInfo[];              // 同类所有已装版本（含 PhpStudy + Store）
  active: ServiceInfo;             // 下拉里当前选中的
  running: ServiceInfo | null;     // 当前同 kind 运行中的那一个（如有）
}

/**
 * 按 kind 分组，每组一张卡。active 的优先级：
 *   1. 用户下拉选的（selectedByKind）
 *   2. 运行中的实例
 *   3. 第一个扫到的
 */
const mainKindGroups = computed<KindGroup[]>(() => {
  const kinds = ["nginx", "apache", "mysql", "redis"];
  const groups: KindGroup[] = [];
  for (const k of kinds) {
    const all = services.value.filter(s => s.kind === k);
    if (all.length === 0) continue;
    const running = all.find(isRunning) ?? null;
    const userPick = all.find(s => s.id === selectedByKind.value[k]);
    const active = userPick ?? running ?? all[0];
    groups.push({ kind: k, all, active, running });
  }
  return groups;
});

function originShort(s: ServiceInfo): string {
  if (s.origin === "phpstudy") return "PS";
  if (s.origin === "manual") return "独立";
  if (s.origin === "system") return "系统";
  return "NX";
}

function versionOptionLabel(s: ServiceInfo, running: ServiceInfo | null): string {
  return `${running?.id === s.id ? "● " : ""}v${s.version} [${originShort(s)}]`;
}

/** 在不同版本运行时，从下拉选了另一个版本 + 点"切换" → 停旧启新 */
async function switchToActive(kind: string, g: KindGroup) {
  if (!g.running || g.running.id === g.active.id) return;
  const targetId = g.active.id;
  const targetLabel = `${kindLabel(kind)} v${g.active.version}`;
  busyIds.value.add(targetId);
  pauseAutoRefresh();
  toast.info(`切换到 ${targetLabel}...`);
  try {
    // start_service 后端的 start_with_deps 会先停同 kind 其他版本
    services.value = await invokeWithTimeout("start_service", { id: targetId });
    toast.success(`已切换到 ${targetLabel}`);
  } catch (e) {
    showError(`切换失败: ${e}`);
    // 回退：清用户选择，让 computed 自然拉回真实运行版本
    delete selectedByKind.value[kind];
  } finally {
    busyIds.value.delete(targetId);
  }
}

const phpServices = computed(() => services.value.filter(s => s.kind === "php"));

const phpRunning = computed(() => phpServices.value.filter(isRunning).length);

/// 任意一个服务在跑就算"运行中"。用于决定全局按钮显示"全部启动"还是"全部停止"。
const anyRunning = computed(() => services.value.some(isRunning));

// ==================== Helpers ====================

function isRunning(s: ServiceInfo): boolean { return s.status.state === "Running"; }
function isBusy(id: string): boolean { return busyIds.value.has(id); }
function originBadge(s: ServiceInfo): { text: string; color: string } | null {
  if (s.origin === "phpstudy") return { text: "PS", color: "var(--color-purple)" };
  if (s.origin === "manual") return { text: "独立", color: "var(--color-cyan)" };
  if (s.origin === "store") return { text: "NX", color: "var(--color-success)" };
  if (s.origin === "system") return { text: "系统", color: "var(--color-accent)" };
  return null;
}

function kindLabel(kind: string): string {
  return ({ nginx: "Nginx", apache: "Apache", mysql: "MySQL", redis: "Redis" } as Record<string, string>)[kind] || kind;
}

function showError(msg: unknown) {
  toast.error(friendlyError(msg));
}

function pauseAutoRefresh() { pauseUntil = Date.now() + 5000; }

const refreshBusy = ref(false);
/// 用户手动刷新：强拉一次完整状态，并暂停自动轮询 5s 避免和下一次定时拉重叠。
async function manualRefresh() {
  if (refreshBusy.value || batchBusy.value) return;
  refreshBusy.value = true;
  pauseAutoRefresh();
  // 后端写一条日志（活动日志里能看到"用户手动刷新"），失败不影响刷新本身
  invoke("log_user_action", { message: "用户手动刷新仪表板" }).catch(() => {});
  try {
    await Promise.all([
      loadServices(true),
      loadAppStats(),
      loadGlobalPhp(),
      loadDevTools(),
      loadRecentLogs(),
      scanStrangers(),
    ]);
  } finally {
    refreshBusy.value = false;
  }
}

// ==================== Data loading ====================

async function loadServices(force = false) {
  if (!force && Date.now() < pauseUntil) return;
  try {
    services.value = await invoke("get_services");
    loaded.value = true;
  } catch (e) { showError("加载失败: " + e); }
}

async function loadRecentLogs() {
  try {
    recentLogs.value = await invokeWithTimeout<LogEntry[]>("get_logs", { limit: 6 });
  } catch (e) {
    const now = Date.now();
    const last = bgErrorAt.get("recent_logs") ?? 0;
    if (now - last > 15000) {
      bgErrorAt.set("recent_logs", now);
      toast.error(`日志刷新失败: ${e}`);
    }
  }
}

async function loadAppStats() {
  try {
    const stats = await invokeWithTimeout<AppStats>("get_app_stats");
    appStats.value = stats;
    localUptime.value = stats.uptime_secs;
  } catch (e) {
    const now = Date.now();
    const last = bgErrorAt.get("app_stats") ?? 0;
    if (now - last > 15000) {
      bgErrorAt.set("app_stats", now);
      toast.error(`资源统计刷新失败: ${e}`);
    }
  }
}

async function checkForUpdates() {
  // getVersion 单独 try：即便 ACL 拒绝，也不阻断 banner 显示
  let current = "";
  try {
    const { getVersion } = await import("@tauri-apps/api/app");
    current = await getVersion();
  } catch { /* 旧版本可能缺 core:app:allow-version 权限，降级 */ }

  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const upd = await check();
    if (upd) {
      updateInfo.value = {
        available: true,
        latest_version: upd.version,
        current_version: current,
        release_url: "",
        release_notes: upd.body ?? "",
        download_url: null,
      };
    } else {
      updateInfo.value = { available: false, latest_version: current, current_version: current, release_url: "", release_notes: "", download_url: null };
    }
  } catch (e) {
    const now = Date.now();
    const last = bgErrorAt.get("updates") ?? 0;
    if (now - last > 30000) {
      bgErrorAt.set("updates", now);
      toast.error(`更新检查失败: ${e}`);
    }
  }
}

// OTA 自更新（Tauri plugin-updater）
const updateBusy = ref(false);
const updateProgress = ref(0); // 0-100
const updateProgressLabel = ref("");

async function performAutoUpdate() {
  if (updateBusy.value) return;
  updateBusy.value = true;
  updateProgress.value = 0;
  updateProgressLabel.value = "检查更新...";
  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const upd = await check();
    if (!upd) {
      toast.info("已是最新版本");
      return;
    }
    let downloaded = 0;
    let total = 0;
    await upd.downloadAndInstall((event) => {
      if (event.event === "Started") {
        total = event.data.contentLength ?? 0;
        updateProgressLabel.value = total > 0
          ? `下载中 0 / ${(total / 1024 / 1024).toFixed(1)} MB`
          : "下载中...";
      } else if (event.event === "Progress") {
        downloaded += event.data.chunkLength ?? 0;
        if (total > 0) {
          updateProgress.value = Math.min(99, Math.round((downloaded / total) * 100));
          updateProgressLabel.value = `下载中 ${(downloaded / 1024 / 1024).toFixed(1)} / ${(total / 1024 / 1024).toFixed(1)} MB`;
        }
      } else if (event.event === "Finished") {
        updateProgress.value = 100;
        updateProgressLabel.value = "安装中...";
      }
    });
    // 安装完成后会自动重启（plugin-updater 默认行为）；这里再补一次手动 relaunch 兜底
    const { relaunch } = await import("@tauri-apps/plugin-process");
    await relaunch();
  } catch (e) {
    showError(`自动更新失败: ${e}`);
  } finally {
    updateBusy.value = false;
  }
}

async function checkStartupErrors() {
  try {
    const errs: string[] = await invokeWithTimeout("get_startup_errors");
    for (const e of errs) showError(e);
  } catch (e) {
    showError(`读取启动错误失败: ${e}`);
  }
}

async function loadGlobalPhp() {
  try {
    globalPhp.value = await invokeWithTimeout<GlobalPhpInfo>("get_global_php_version");
  } catch (e) {
    const now = Date.now();
    const last = bgErrorAt.get("global_php") ?? 0;
    if (now - last > 15000) {
      bgErrorAt.set("global_php", now);
      toast.error(`读取全局 PHP 失败: ${e}`);
    }
  }
}

// ==================== Dev Tools ====================

interface ComposerOption { version: string; source: string; phar_path: string; }
interface ComposerToolInfo { active_version: string | null; available: ComposerOption[]; }
interface NvmToolInfo { nvm_version: string; nvm_source: string; nvm_home: string; current_node: string | null; installed_nodes: string[]; }
interface MysqlOption { version: string; install_path: string; data_dir: string; bin_dir: string; port: number; initialized: boolean; root_password: string; }
interface MysqlToolInfo { active_version: string | null; available: MysqlOption[]; }
interface DevToolsInfo { composer: ComposerToolInfo | null; nvm: NvmToolInfo | null; mysql: MysqlToolInfo | null; }

const devTools = ref<DevToolsInfo>({ composer: null, nvm: null, mysql: null });

async function loadDevTools() {
  try {
    devTools.value = await invokeWithTimeout<DevToolsInfo>("get_dev_tools_info");
  } catch (e) {
    const now = Date.now();
    const last = bgErrorAt.get("dev_tools") ?? 0;
    if (now - last > 15000) {
      bgErrorAt.set("dev_tools", now);
      toast.error(`读取开发工具信息失败: ${e}`);
    }
  }
}

// ==================== Actions ====================

// 端口冲突诊断对话框 state
const portConflict = ref<{
  serviceId: string; serviceName: string; port: number;
  pid: number; processName: string; exePath: string;
} | null>(null);
const portConflictBusy = ref(false);

/// 解析后端"端口 X 已被外部进程占用 (PID Y, Z)"格式错误
function parsePortConflict(err: string): { port: number; pid: number; exe: string } | null {
  const m = err.match(/端口\s*(\d+)\s*已被外部进程占用\s*\(PID\s*(\d+),\s*([^)]+)\)/);
  if (!m) return null;
  return { port: parseInt(m[1]), pid: parseInt(m[2]), exe: m[3].trim() };
}

/// 端口冲突 → 弹诊断对话框；否则返回 false 让调用方按普通错误处理。
function tryShowPortConflict(id: string, err: unknown): boolean {
  const conflict = parsePortConflict(String(err));
  if (!conflict) return false;
  const svc = services.value.find(s => s.id === id);
  const exeBase = conflict.exe.split(/[\\/]/).pop() || conflict.exe;
  portConflict.value = {
    serviceId: id,
    serviceName: svc ? `${svc.kind} v${svc.version}` : id,
    port: conflict.port,
    pid: conflict.pid,
    processName: exeBase,
    exePath: conflict.exe,
  };
  return true;
}

async function startService(id: string) {
  busyIds.value.add(id); pauseAutoRefresh();
  try {
    services.value = await invokeWithTimeout("start_service", { id });
  } catch (e) {
    if (!tryShowPortConflict(id, e)) showError("启动失败: " + e);
  } finally { busyIds.value.delete(id); }
}

async function killAndRetryStart() {
  if (!portConflict.value) return;
  portConflictBusy.value = true;
  try {
    await invokeWithTimeout("kill_process_by_pid", { pid: portConflict.value.pid });
    toast.success(`已结束进程 PID ${portConflict.value.pid}`);
    const sid = portConflict.value.serviceId;
    portConflict.value = null;
    // 等 1 秒让端口释放
    await new Promise((r) => setTimeout(r, 1000));
    busyIds.value.add(sid); pauseAutoRefresh();
    try {
      services.value = await invokeWithTimeout("start_service", { id: sid });
      toast.success("服务已启动");
    } catch (e2) {
      showError("重试启动仍失败: " + e2);
    } finally {
      busyIds.value.delete(sid);
    }
  } catch (e) {
    showError("结束进程失败: " + e);
  } finally {
    portConflictBusy.value = false;
  }
}

async function stopService(id: string) {
  busyIds.value.add(id); pauseAutoRefresh();
  try {
    services.value = await invokeWithTimeout("stop_service", { id });
  } catch (e) { showError("停止失败: " + e); }
  finally { busyIds.value.delete(id); }
}

async function restartService(id: string) {
  busyIds.value.add(id); pauseAutoRefresh();
  try {
    services.value = await invokeWithTimeout("restart_service", { id });
  } catch (e) {
    // 重启时 stop 已成功但 start 阶段端口被抢 → 走同一个端口冲突诊断流程
    if (!tryShowPortConflict(id, e)) showError("重启失败: " + e);
  } finally { busyIds.value.delete(id); }
}

async function startAll() {
  if (batchBusy.value) return;
  batchBusy.value = true; pauseAutoRefresh();
  try { services.value = await invokeWithTimeout("start_all"); }
  catch (e) { showError("全部启动失败: " + e); }
  finally { batchBusy.value = false; }
}

async function stopAll() {
  if (batchBusy.value) return;
  batchBusy.value = true; pauseAutoRefresh();
  try { services.value = await invokeWithTimeout("stop_all"); }
  catch (e) { showError("全部停止失败: " + e); }
  finally { batchBusy.value = false; }
}

// ==================== Log display ====================

const levelIconMap: Record<string, any> = {
  debug: Bug, info: Info, success: CheckCircle2, warn: AlertTriangle, error: AlertCircle,
};
function levelColor(level: string): string {
  return ({
    debug: "var(--text-muted)",
    info: "var(--color-blue-light)",
    success: "var(--color-success-light)",
    warn: "var(--color-yellow)",
    error: "var(--color-danger)",
  } as Record<string, string>)[level] || "var(--text-muted)";
}

onMounted(async () => {
  // 立即异步加载，不 await，让 skeleton 先渲染出来
  loadServices(true);
  checkStartupErrors();
  loadRecentLogs();
  loadGlobalPhp();
  loadDevTools();
  loadAppStats();
  checkForUpdates();
  // 异步扫描占用关键端口的外部进程（不阻塞首屏）
  scanStrangers();
  // 后端在装/卸 / rescan 完成后推 services-changed 事件 → 立即刷新，不等轮询
  unlistenServicesChanged = await listen("services-changed", () => {
    loadServices(true);
    loadGlobalPhp();
    loadDevTools();
  });
  // GlobalEnv 页面切换全局 PHP/Composer/Node/MySQL/镜像后会发该事件 → 刷新仪表板的环境概览
  // （Dashboard 被 KeepAlive 缓存，路由切回时不会重新 onMounted，必须靠事件同步）
  unlistenGlobalEnvChanged = await listen("global-env-changed", () => {
    loadGlobalPhp();
    loadDevTools();
  });
  // 批量启动/停止时后端逐服务推送状态，卡片不再等整个批处理结束才变化。
  unlistenBatchProgress = await listen<ServiceBatchProgress>("service-batch-progress", (event) => {
    const idx = services.value.findIndex((service) => service.id === event.payload.id);
    if (idx >= 0) {
      services.value[idx] = {
        ...services.value[idx],
        status: event.payload.status,
      };
    }
  });
  // 轮询从 3s 放宽到 5s（后端已做 status 缓存 + 快速返回 + 后台并行刷新）
  svcTimer = window.setInterval(() => {
    loadServices();
    loadAppStats();  // 顺带刷新本应用内存/运行时长
  }, 5000);
  logTimer = window.setInterval(loadRecentLogs, 2000);
  uptimeTimer = window.setInterval(() => { localUptime.value++; }, 1000);
});

onUnmounted(() => {
  if (svcTimer) clearInterval(svcTimer);
  if (logTimer) clearInterval(logTimer);
  if (uptimeTimer) clearInterval(uptimeTimer);
  if (unlistenServicesChanged) unlistenServicesChanged();
  if (unlistenGlobalEnvChanged) unlistenGlobalEnvChanged();
  if (unlistenBatchProgress) unlistenBatchProgress();
});
</script>

<template>
  <div class="max-w-[1100px]">
    <!-- Header -->
    <div class="flex items-center justify-end mb-3">
      <div class="flex gap-2">
        <button class="btn btn-secondary btn-sm" :disabled="refreshBusy || batchBusy" @click="manualRefresh" title="刷新">
          <RefreshCw :size="14" :class="{ 'animate-spin': refreshBusy }" />
        </button>
        <button v-if="anyRunning" class="btn btn-danger btn-sm" :disabled="batchBusy" @click="stopAll">{{ batchBusy ? "停止中..." : "全部停止" }}</button>
        <button v-else class="btn btn-success btn-sm" :disabled="batchBusy" @click="startAll">{{ batchBusy ? "启动中..." : "全部启动" }}</button>
      </div>
    </div>

    <!-- 外部进程提醒：占着关键端口但不在我们管理的安装目录里的进程 -->
    <div v-for="s in strangers" :key="s.pid" class="stranger-banner mb-2">
      <AlertTriangle :size="16" class="shrink-0" style="color: var(--color-warn)" />
      <span class="text-[16px] flex-1 min-w-0 truncate">
        检测到外部 <b>{{ s.kind }}</b> 占用端口 <b>{{ s.port }}</b>
        <span class="font-mono text-[13px] ml-1 opacity-70">{{ s.exe_path }} (PID {{ s.pid }})</span>
      </span>
      <div class="flex gap-1.5 shrink-0">
        <button class="btn btn-danger btn-sm"
                :disabled="strangerKilling.has(s.pid)"
                @click="killStranger(s)">
          {{ strangerKilling.has(s.pid) ? '结束中...' : '结束进程' }}
        </button>
        <button class="btn btn-secondary btn-sm" @click="dismissStranger(s)">忽略</button>
      </div>
    </div>

    <!-- 更新通知横条 -->
    <div v-if="updateInfo?.available && !updateDismissed" class="update-banner mb-3">
      <span class="text-[16px]">
        <template v-if="!updateBusy">新版本 <b>v{{ updateInfo.latest_version }}</b> 可用（当前 v{{ updateInfo.current_version }}）</template>
        <template v-else>{{ updateProgressLabel }} {{ updateProgress }}%</template>
      </span>
      <div class="flex gap-1.5 ml-auto">
        <button class="btn btn-primary btn-sm" :disabled="updateBusy" @click="performAutoUpdate">
          {{ updateBusy ? "更新中…" : "立即更新" }}
        </button>
        <button class="btn btn-secondary btn-sm" :disabled="updateBusy" @click="updateDismissed = true">稍后</button>
      </div>
    </div>

    <!-- Skeleton：首次加载前占位，避免空白页闪烁 -->
    <div v-if="!loaded" class="grid grid-cols-4 gap-3 mb-4">
      <div v-for="i in 4" :key="i" class="svc-card skeleton-card">
        <div class="flex items-start justify-between mb-3">
          <div class="min-w-0 flex-1">
            <div class="skel-bar h-3 w-14 mb-1.5"></div>
            <div class="skel-bar h-2 w-10"></div>
          </div>
          <span class="w-2 h-2 rounded-full shrink-0 mt-1.5" style="background: var(--color-gray-light); opacity: 0.4"></span>
        </div>
        <div class="skel-bar h-3 w-16 mb-3"></div>
        <div class="skel-bar h-7 w-full rounded"></div>
      </div>
    </div>

    <!-- Empty State -->
    <div v-else-if="services.length === 0" class="card py-8 px-6">
      <div class="text-center mb-5">
        <div class="inline-flex w-12 h-12 rounded-full items-center justify-center mb-3"
             style="background: var(--bg-tertiary); color: var(--text-muted)">
          <Store :size="22" />
        </div>
        <p class="text-base font-semibold mb-1">没有发现已安装的服务</p>
        <p class="text-[16px]" style="color: var(--text-muted)">选择一个方式继续：</p>
      </div>
      <div class="grid grid-cols-2 gap-3 max-w-[560px] mx-auto">
        <button class="cta-card" @click="router.push('/store')">
          <Store :size="18" class="mb-2" style="color: var(--color-purple)" />
          <span class="font-semibold text-[16px] mb-0.5">打开软件商店</span>
          <span class="text-[13px]" style="color: var(--text-muted)">一键安装 Nginx / MySQL / PHP / Redis</span>
        </button>
        <button class="cta-card" @click="router.push('/settings')">
          <Settings2 :size="18" class="mb-2" style="color: var(--color-blue-light)" />
          <span class="font-semibold text-[16px] mb-0.5">指定 PHPStudy 路径</span>
          <span class="text-[13px]" style="color: var(--text-muted)">已安装过 PHPStudy？设置里填路径</span>
        </button>
      </div>
    </div>

    <template v-else>
      <!-- Main service cards (2 columns) -->
      <div class="grid grid-cols-2 gap-3 mb-4">
        <div v-for="g in mainKindGroups" :key="g.kind"
             class="svc-card"
             :class="{ 'is-running': !!g.running }">
          <!-- Name & Status -->
          <div class="flex items-start justify-between mb-3">
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-1.5">
                <div class="text-[16px] font-semibold">{{ kindLabel(g.kind) }}</div>
                <span v-if="originBadge(g.active)" class="text-[9px] px-1.5 py-px rounded font-semibold leading-none"
                      :style="{ background: `${originBadge(g.active)!.color}22`, color: originBadge(g.active)!.color }">{{ originBadge(g.active)!.text }}</span>
              </div>
              <!-- 单版本：静态显示；多版本：下拉（纯选择，不触发 IPC） -->
              <template v-if="g.all.length === 1">
                <div class="text-[13px] font-mono mt-0.5" style="color: var(--text-muted)">{{ g.active.version }}</div>
              </template>
              <template v-else>
                <SelectMenu
                  :model-value="g.active.id"
                  :options="g.all.map((s) => ({ label: versionOptionLabel(s, g.running), value: s.id }))"
                  trigger-class="version-sel version-sel-btn mt-1"
                  @update:modelValue="selectedByKind[g.kind] = String($event)"
                />
              </template>
            </div>
            <span class="w-2 h-2 rounded-full shrink-0 mt-1.5 transition-all duration-300"
                  :style="g.running
                    ? { background: 'var(--color-success-light)', boxShadow: '0 0 8px var(--color-success-light)' }
                    : { background: 'var(--color-gray-light)' }"></span>
          </div>

          <!-- Status + Actions (one row) -->
          <div class="flex items-center justify-between">
            <div class="text-[16px] transition-colors"
                 :style="{ color: g.running && g.running.id === g.active.id ? 'var(--color-success-light)' : 'var(--text-muted)' }">
              <template v-if="isBusy(g.active.id)">操作中...</template>
              <template v-else-if="g.active.status.state === 'Starting'">启动中...</template>
              <template v-else-if="g.active.status.state === 'Stopping'">停止中...</template>
              <template v-else-if="g.active.status.state === 'Failed'">操作失败</template>
              <template v-else-if="!g.running">已停止</template>
              <template v-else-if="g.running.id === g.active.id">运行中</template>
              <template v-else>v{{ g.running.version }} 运行中</template>
            </div>
            <div class="flex items-center gap-2">
              <button v-if="g.running && g.running.id === g.active.id && !isBusy(g.active.id)"
                      class="svc-action-btn" title="重启"
                      @click="restartService(g.active.id)">⟳</button>
              <button v-if="g.running && g.running.id !== g.active.id && !isBusy(g.active.id)"
                      class="btn btn-primary btn-xs" @click="switchToActive(g.kind, g)">切换</button>
              <div class="svc-toggle"
                   :class="{ active: g.running && g.running.id === g.active.id, disabled: isBusy(g.active.id) }"
                   @click="g.running && g.running.id === g.active.id ? stopService(g.active.id) : startService(g.active.id)">
                <span class="svc-toggle-slider"></span>
              </div>
            </div>
          </div>
        </div>
      </div>

      <!-- PHP engine combined card -->
      <div v-if="phpServices.length" class="card mb-4">
        <div class="flex items-center justify-between mb-3">
          <div class="text-[16px] font-semibold">PHP 引擎</div>
          <span class="text-[13px]" style="color: var(--text-muted)">
            <span style="color: var(--color-success-light)" class="font-semibold">{{ phpRunning }}</span>/{{ phpServices.length }} 运行中
          </span>
        </div>

        <div class="flex flex-wrap items-center gap-1.5">
          <div v-for="p in phpServices" :key="p.id"
               class="php-chip"
               :class="{ 'is-running': isRunning(p) }">
            <span class="w-1.5 h-1.5 rounded-full shrink-0"
                  :style="isRunning(p)
                    ? { background: 'var(--color-success-light)', boxShadow: '0 0 4px var(--color-success-light)' }
                    : { background: 'var(--color-gray-light)' }"></span>
            <span class="font-mono">{{ p.version }}{{ p.variant ? ' '+p.variant : '' }}</span>
            <span class="opacity-50 text-[9px]">{{ originShort(p) }}</span>
          </div>
        </div>
      </div>

      <!-- 全局环境只读摘要（编辑去服务配置 → 全局环境 tab） -->
      <div class="env-summary mb-4" @click="router.push({ path: '/config', query: { tab: 'env' } })">
        <span class="env-summary-label">全局环境</span>
        <span class="env-summary-item">PHP <b>{{ globalPhp.version ? `v${globalPhp.version}` : '—' }}</b></span>
        <span class="env-summary-item">Composer <b>{{ devTools.composer?.active_version ? `v${devTools.composer.active_version}` : '—' }}</b></span>
        <span class="env-summary-item">Node <b>{{ devTools.nvm?.current_node ? `v${devTools.nvm.current_node}` : '—' }}</b></span>
        <span class="env-summary-item">MySQL <b>{{ devTools.mysql?.active_version ? `v${devTools.mysql.active_version}` : '—' }}</b></span>
        <ChevronRight :size="14" class="ml-auto" />
      </div>

      <!-- Recent Activity Log (compact) -->
      <div class="rounded-lg px-3 py-2" style="background: var(--bg-secondary); box-shadow: var(--shadow-card)">
        <div class="flex items-center justify-between mb-1.5">
          <div class="text-[13px] font-semibold uppercase tracking-wider" style="color: var(--text-muted)">活动日志</div>
          <button class="text-[13px] flex items-center gap-0.5 hover:opacity-80 transition-opacity cursor-pointer"
                  style="color: var(--color-blue-light); background: transparent; border: none"
                  @click="logDrawerOpen = true">
            查看全部 <ChevronRight :size="11" />
          </button>
        </div>
        <div v-if="recentLogs.length === 0" class="text-[13px] py-2 text-center" style="color: var(--text-muted)">暂无活动</div>
        <div v-else class="flex flex-col">
          <div v-for="log in recentLogs.slice(0, 5)" :key="log.id"
               class="flex items-center gap-2 px-1 py-0.5 rounded text-[13px]">
            <component :is="levelIconMap[log.level]" :size="11" :style="{ color: levelColor(log.level) }" class="shrink-0" />
            <span class="font-mono text-[13px] shrink-0" style="color: var(--text-muted)">{{ log.timestamp.slice(11, 19) }}</span>
            <span class="truncate" style="color: var(--text-secondary)">{{ log.message }}</span>
          </div>
        </div>
      </div>

      <!-- 应用自身状态条 -->
      <div v-if="appStats" class="flex items-center gap-3 mt-2 text-[13px]" style="color: var(--text-muted)">
        <span>{{ APP_NAME }} · PID {{ appStats.pid }}</span>
        <span v-if="appStats.memory_mb != null">· 内存 {{ appStats.memory_mb }} MB</span>
        <span>· 运行 {{ fmtUptime(localUptime) }}</span>
      </div>
    </template>

    <!-- Log Drawer -->
    <LogDrawer :open="logDrawerOpen" @close="logDrawerOpen = false" />

    <!-- 端口冲突自动诊断对话框 -->
    <ConfirmDialog
      :open="!!portConflict"
      title="启动失败：端口被占用"
      variant="danger"
      confirm-text="结束进程并重试"
      :busy="portConflictBusy"
      @confirm="killAndRetryStart"
      @cancel="portConflict = null"
    >
      <template v-if="portConflict">
        <b>{{ portConflict.serviceName }}</b> 启动失败：端口 <b>{{ portConflict.port }}</b> 被以下进程占用：
        <div style="margin-top: 8px; padding: 8px 10px; background: var(--bg-tertiary); border-radius: 6px; border: 1px solid var(--border-color); font-family: var(--font-mono); font-size: 12px">
          <div>{{ portConflict.processName }} (PID {{ portConflict.pid }})</div>
          <div style="color: var(--text-muted); font-size: 11px; margin-top: 2px; word-break: break-all">{{ portConflict.exePath }}</div>
        </div>
        <div style="margin-top: 8px; color: var(--text-muted); font-size: 12px">
          点击"结束进程并重试"会先结束该进程（如 PHPStudy 自带的 nginx），等待 1 秒后再次启动 NaxOne 的服务。
        </div>
      </template>
    </ConfirmDialog>
  </div>
</template>

<style scoped>
.svc-card {
  background: var(--bg-secondary);
  backdrop-filter: var(--bg-glass-blur);
  -webkit-backdrop-filter: var(--bg-glass-blur);
  border: 1px solid var(--border-color);
  border-radius: 12px;
  padding: 16px;
  box-shadow: var(--shadow-card);
  transition: box-shadow 200ms ease, border-color 200ms ease;
}
.svc-card:hover {
  box-shadow: 0 4px 14px rgba(0, 0, 0, 0.22);
  border-color: var(--border-color-hover, var(--border-color));
}

/* Service toggle switch */
.svc-toggle {
  position: relative;
  display: inline-flex;
  align-items: center;
  cursor: pointer;
}
.svc-toggle.disabled { cursor: not-allowed; opacity: 0.5; pointer-events: none; }
.svc-toggle-slider {
  position: relative;
  width: 44px;
  height: 24px;
  border-radius: 12px;
  background: var(--color-gray-light);
  transition: background 250ms ease;
}
.svc-toggle-slider::after {
  content: "";
  position: absolute;
  top: 3px;
  left: 3px;
  width: 18px;
  height: 18px;
  border-radius: 50%;
  background: #fff;
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.3);
  transition: transform 250ms cubic-bezier(0.4, 0, 0.2, 1);
}
.svc-toggle.active .svc-toggle-slider {
  background: var(--color-success);
}
.svc-toggle.active .svc-toggle-slider::after {
  transform: translateX(20px);
}

/* Restart action button */
.svc-action-btn {
  width: 28px;
  height: 28px;
  border-radius: 6px;
  border: 1px solid var(--border-color);
  background: var(--bg-tertiary);
  color: var(--text-secondary);
  font-size: 14px;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  transition: all 150ms ease;
}
.svc-action-btn:hover {
  background: var(--bg-hover);
  color: var(--text-primary);
  border-color: var(--text-muted);
}
/* PHP version chips */
.php-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 8px 13px;
  border-radius: 999px;
  font-size: 13px;
  background: var(--bg-tertiary);
  color: var(--text-secondary);
  transition: all 0.2s;
}
.php-chip.is-running {
  background: rgba(74, 222, 128, 0.08);
  color: var(--color-success-light);
}

/* 外部进程提醒条 */
.stranger-banner {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 12px;
  border-radius: 8px;
  background: rgba(245, 158, 11, 0.08);
  border: 1px solid rgba(245, 158, 11, 0.35);
}

/* 更新提醒横条 */
.update-banner {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 14px;
  border-radius: 8px;
  background: rgba(99, 128, 255, 0.1);
  border: 1px solid rgba(99, 128, 255, 0.35);
}

/* 系统 PATH 冲突警告条 */
.conflict-banner {
  background: rgba(245, 158, 11, 0.08);
  border: 1px solid rgba(245, 158, 11, 0.35);
  border-radius: 8px;
  padding: 10px 12px;
}
.conflict-banner code {
  padding: 1px 4px;
  background: var(--bg-tertiary);
  border-radius: 3px;
  font-size: 13px;
}

.cta-card {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  text-align: center;
  padding: 18px 14px;
  border-radius: 10px;
  background: var(--bg-secondary);
  backdrop-filter: var(--bg-glass-blur);
  -webkit-backdrop-filter: var(--bg-glass-blur);
  border: 1px solid var(--border-color);
  cursor: pointer;
  transition: transform 180ms ease, box-shadow 180ms ease, border-color 180ms ease;
  color: var(--text-primary);
}
.cta-card:hover:not(:disabled) {
  transform: translateY(-2px);
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.22);
  border-color: var(--color-blue);
}
.cta-card:disabled {
  cursor: not-allowed;
  opacity: 0.55;
}

/* 全局环境只读摘要：1 行卡片，点击跳转 /env */
.env-summary {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 14px 18px;
  border-radius: 12px;
  background: var(--bg-secondary);
  backdrop-filter: var(--bg-glass-blur);
  -webkit-backdrop-filter: var(--bg-glass-blur);
  box-shadow: var(--shadow-card);
  cursor: pointer;
  color: var(--text-secondary);
  transition: background var(--transition);
}
.env-summary:hover {
  background: var(--bg-hover);
}
.env-summary-label {
  font-size: 13px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-muted);
}
.env-summary-item {
  font-size: 13px;
  color: var(--text-secondary);
}
.env-summary-item b {
  color: var(--text-primary);
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  font-weight: 600;
}

/* 版本下拉：默认会被 flex-1 父容器撑满，限制成内容宽度 + 上限 */
:deep(.rs-select.version-sel) {
  display: inline-flex;
  width: auto;
  max-width: 100%;
}
:deep(.rs-select-trigger.version-sel-btn) {
  width: auto;
  min-width: 110px;
  max-width: 180px;
}

/* Skeleton 占位：首次加载前显示，shimmer 动画提示"正在准备" */
.skeleton-card {
  pointer-events: none;
  opacity: 0.75;
}
.skel-bar {
  display: block;
  border-radius: 4px;
  background: linear-gradient(
    90deg,
    var(--bg-tertiary) 0%,
    var(--bg-hover) 50%,
    var(--bg-tertiary) 100%
  );
  background-size: 200% 100%;
  animation: skel-shimmer 1.4s ease-in-out infinite;
}
@keyframes skel-shimmer {
  0% { background-position: 200% 0; }
  100% { background-position: -200% 0; }
}
</style>
