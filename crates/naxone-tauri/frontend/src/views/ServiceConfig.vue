<script setup lang="ts">
import { ref, watch, onMounted, computed, nextTick } from "vue";
import { useRoute, useRouter } from "vue-router";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { toast, friendlyError } from "../composables/useToast";
import { onTextareaTab } from "../composables/useTextareaTab";
import SelectMenu from "../components/SelectMenu.vue";
import GlobalEnv from "./GlobalEnv.vue";
import { Plus, Search, Download, Copy } from "lucide-vue-next";

type Tab = "nginx" | "mysql" | "redis" | "php" | "hosts" | "env";
const route = useRoute();
const router = useRouter();
const activeTab = ref<Tab>("env");

function setTab(t: Tab) {
  activeTab.value = t;
  // 同步到 URL；env 是默认 tab，省略 query 让 URL 更干净
  router.replace({ path: "/config", query: t === "env" ? {} : { tab: t } });
}
const busy = ref(false);
const saved = ref(false);
const boolOptions = [
  { label: "开启", value: true },
  { label: "关闭", value: false },
];
const mysqlFlushOptions = [
  { label: "0 - 每秒刷新", value: 0 },
  { label: "1 - 每次提交(最安全)", value: 1 },
  { label: "2 - 写日志每秒刷盘", value: 2 },
];
const mysqlCharsetOptions = [
  { label: "utf8", value: "utf8" },
  { label: "utf8mb4", value: "utf8mb4" },
  { label: "latin1", value: "latin1" },
  { label: "gbk", value: "gbk" },
];
const mysqlEngineOptions = [
  { label: "InnoDB", value: "InnoDB" },
  { label: "MyISAM", value: "MyISAM" },
];
const mysqlLogVerbosityOptions = [
  { label: "1 - 仅错误", value: 1 },
  { label: "2 - 错误+警告", value: 2 },
  { label: "3 - 全部", value: 3 },
];
const redisPolicyOptions = [
  { label: "noeviction", value: "noeviction" },
  { label: "allkeys-lru", value: "allkeys-lru" },
  { label: "volatile-lru", value: "volatile-lru" },
  { label: "allkeys-random", value: "allkeys-random" },
  { label: "volatile-random", value: "volatile-random" },
  { label: "volatile-ttl", value: "volatile-ttl" },
];
const redisAppendfsyncOptions = [
  { label: "everysec", value: "everysec" },
  { label: "always", value: "always" },
  { label: "no", value: "no" },
];
const redisLoglevelOptions = [
  { label: "debug", value: "debug" },
  { label: "verbose", value: "verbose" },
  { label: "notice", value: "notice" },
  { label: "warning", value: "warning" },
];
const phpSessionHandlerOptions = [
  { label: "files", value: "files" },
  { label: "redis", value: "redis" },
  { label: "memcached", value: "memcached" },
];
const phpSameSiteOptions = [
  { label: "不设置", value: "" },
  { label: "Lax", value: "Lax" },
  { label: "Strict", value: "Strict" },
  { label: "None", value: "None" },
];

function showError(msg: unknown) { toast.error(friendlyError(msg)); }
function showSaved() { saved.value = true; setTimeout(() => (saved.value = false), 2000); }

// ======================== Nginx ========================
interface NginxConfig {
  worker_processes: string; worker_connections: number; worker_rlimit_nofile: number;
  keepalive_timeout: number; keepalive_requests: number;
  sendfile: boolean; sendfile_max_chunk: string;
  client_max_body_size: string; client_body_buffer_size: string; client_body_timeout: number;
  client_header_buffer_size: string; client_header_timeout: number; large_client_header_buffers: string;
  send_timeout: number; reset_timedout_connection: boolean;
  server_names_hash_bucket_size: number;
  gzip: boolean; gzip_level: number; gzip_min_length: number;
}
const nginx = ref<NginxConfig | null>(null);
async function loadNginx() { try { nginx.value = await invoke("get_nginx_config"); } catch (e) { showError("加载 Nginx 配置失败: " + e); } }
async function saveNginx() {
  if (!nginx.value || busy.value) return;
  // 防呆：常见数值字段范围校验。让用户在保存前发现，而不是 nginx reload 时报 emerg。
  const n = nginx.value;
  if (n.gzip_level < 1 || n.gzip_level > 9) { showError("Gzip 等级必须在 1-9 之间"); return; }
  if (n.gzip_min_length < 0) { showError("Gzip 最小压缩体积不能为负"); return; }
  if (n.worker_connections < 1) { showError("最大连接数必须 ≥ 1"); return; }
  if (n.worker_rlimit_nofile < 1) { showError("文件描述符限制必须 ≥ 1"); return; }
  if (n.keepalive_timeout < 0) { showError("保活超时不能为负"); return; }
  if (n.keepalive_requests < 1) { showError("单连接最大请求数必须 ≥ 1"); return; }
  busy.value = true;
  try { await invoke("save_nginx_config", { cfg: nginx.value }); showSaved(); }
  catch (e) { showError("保存失败: " + e); }
  finally { busy.value = false; }
}

// ======================== MySQL ========================
interface MysqlConfig {
  port: number; max_connections: number; max_allowed_packet: string;
  innodb_buffer_pool_size: string; innodb_log_file_size: string; innodb_log_buffer_size: string;
  innodb_flush_log_at_trx_commit: number; innodb_lock_wait_timeout: number;
  character_set_server: string; collation_server: string; init_connect: string;
  key_buffer_size: string; table_open_cache: number; tmp_table_size: string; max_heap_table_size: string;
  interactive_timeout: number; wait_timeout: number;
  sort_buffer_size: string; read_buffer_size: string; read_rnd_buffer_size: string;
  join_buffer_size: string; myisam_sort_buffer_size: string;
  thread_cache_size: number; log_error_verbosity: number; default_storage_engine: string;
}
const mysql = ref<MysqlConfig | null>(null);
async function loadMysql() { try { mysql.value = await invoke("get_mysql_config"); } catch (e) { showError("加载 MySQL 配置失败: " + e); } }
async function saveMysql() { if (!mysql.value || busy.value) return; busy.value = true; try { await invoke("save_mysql_config", { cfg: mysql.value }); showSaved(); } catch (e) { showError("保存失败: " + e); } finally { busy.value = false; } }

// ======================== Redis ========================
interface RedisConfig {
  port: number; bind: string; timeout: number; databases: number; maxmemory: string;
  maxclients: number; maxmemory_policy: string; tcp_backlog: number; tcp_keepalive: number;
  loglevel: string; save_rules: string; rdbcompression: boolean; appendonly: boolean;
  appendfsync: string; requirepass: string;
  logfile: string; rdbchecksum: boolean; dbfilename: string; dir: string; stop_writes_on_bgsave_error: boolean;
}
const redis = ref<RedisConfig | null>(null);
async function loadRedis() { try { redis.value = await invoke("get_redis_config"); } catch (e) { showError("加载 Redis 配置失败: " + e); } }
async function saveRedis() { if (!redis.value || busy.value) return; busy.value = true; try { await invoke("save_redis_config", { cfg: redis.value }); showSaved(); } catch (e) { showError("保存失败: " + e); } finally { busy.value = false; } }

// ======================== PHP ========================
interface PhpInstance { id: string; label: string; version: string; variant: string | null; install_path: string; }
interface PhpExtension { name: string; file_name: string; enabled: boolean; is_zend: boolean; }
interface PhpIniSettings {
  memory_limit: string; upload_max_filesize: string; post_max_size: string;
  max_execution_time: number; max_input_time: number; display_errors: boolean;
  error_reporting: string; date_timezone: string; file_uploads: boolean; short_open_tag: boolean;
  allow_url_fopen: boolean; allow_url_include: boolean; disable_functions: string;
  expose_php: boolean; open_basedir: string; opcache_enable: boolean;
  opcache_memory_consumption: number; opcache_max_accelerated_files: number;
  opcache_validate_timestamps: boolean; opcache_revalidate_freq: number;
  session_save_handler: string; session_save_path: string;
  session_gc_maxlifetime: number; session_cookie_lifetime: number;
  session_name: string; session_use_cookies: boolean; session_use_only_cookies: boolean;
  session_use_strict_mode: boolean; session_cookie_httponly: boolean; session_cookie_samesite: string;
  output_buffering: string; default_charset: string; max_file_uploads: number; default_socket_timeout: number;
}

const phpInstances = ref<PhpInstance[]>([]);
const selectedPhp = ref<PhpInstance | null>(null);
const phpExts = ref<PhpExtension[]>([]);
const phpIni = ref<PhpIniSettings | null>(null);
const phpSubTab = ref<"extensions" | "settings" | "phpinfo">("extensions");

// phpinfo（HTML 模式，iframe srcdoc 隔离渲染）
const phpinfoHtml = ref<string>("");
const phpinfoFileUrl = ref<string>("");
const phpinfoError = ref<string>("");
const phpinfoLoading = ref(false);

async function loadPhpInstances() {
  try {
    phpInstances.value = await invoke("get_php_instances");
    if (phpInstances.value.length > 0 && !selectedPhp.value) selectedPhp.value = phpInstances.value[0];
  } catch (e) { showError("加载 PHP 版本失败: " + e); }
}
async function loadPhpExts() {
  if (!selectedPhp.value) return;
  try { phpExts.value = await invoke("get_php_extensions", { installPath: selectedPhp.value.install_path }); } catch (e) { showError("加载扩展失败: " + e); }
}
async function loadPhpIni() {
  if (!selectedPhp.value) return;
  try { phpIni.value = await invoke("get_php_ini_settings", { installPath: selectedPhp.value.install_path }); } catch (e) { showError("加载配置失败: " + e); }
}
async function toggleExt(ext: PhpExtension) {
  if (!selectedPhp.value || busy.value) return;
  busy.value = true;
  try { phpExts.value = await invoke("toggle_php_extension", { installPath: selectedPhp.value.install_path, extName: ext.name, enable: !ext.enabled, isZend: ext.is_zend }); } catch (e) { showError("切换失败: " + e); } finally { busy.value = false; }
}
async function savePhpIni() {
  if (!selectedPhp.value || !phpIni.value || busy.value) return;
  busy.value = true;
  try { await invoke("save_php_ini_settings", { installPath: selectedPhp.value.install_path, settings: phpIni.value }); showSaved(); } catch (e) { showError("保存失败: " + e); } finally { busy.value = false; }
}
async function loadPhpInfo() {
  if (!selectedPhp.value || phpinfoLoading.value) return;
  phpinfoLoading.value = true;
  phpinfoError.value = "";
  phpinfoHtml.value = "";
  phpinfoFileUrl.value = "";
  try {
    const res = await invoke<{ html: string; file_url: string }>("get_phpinfo_html", {
      installPath: selectedPhp.value.install_path,
    });
    phpinfoHtml.value = res.html;
    phpinfoFileUrl.value = res.file_url;
  } catch (e) {
    phpinfoError.value = String(e);
  } finally { phpinfoLoading.value = false; }
}
async function openPhpInfoInBrowser() {
  if (!phpinfoFileUrl.value) return;
  try { await invoke("open_in_browser", { url: phpinfoFileUrl.value }); }
  catch (e) { showError(`打开浏览器失败: ${e}`); }
}
function switchPhpSubTab(t: typeof phpSubTab.value) {
  phpSubTab.value = t;
  if (t === "phpinfo" && !phpinfoHtml.value && !phpinfoError.value) loadPhpInfo();
}

watch(selectedPhp, () => {
  loadPhpExts();
  loadPhpIni();
  phpinfoHtml.value = "";
  phpinfoFileUrl.value = "";
  phpinfoError.value = "";
  // 当前正在 phpinfo tab 时切换版本要立刻刷新，否则只在下次进 tab 时拉取
  if (phpSubTab.value === "phpinfo") loadPhpInfo();
});

// ─── PIE 扩展安装 ───────────────────────────────────
interface PieExtensionInfo { name: string; description: string }
interface PieRuntimeInfo { runtime_php_path: string | null; runtime_version: string | null }
interface RecommendedExt { name: string; ext: string; display: string; description: string; category: string; minPhp: string; maxPhp?: string }

// 精选 Windows 可用扩展。
// 现状：PIE 在 Windows 上能装的扩展非常少 —— 主流扩展（redis/imagick/mongodb 等）在 GitHub
// release 不附 Windows binary，靠 windows.php.net/downloads/pecl/ 镜像分发，PIE 暂未接管这条路。
// 实测唯一全 PHP 版本可装的精选扩展是 xdebug（它是 PIE 命名约定的标杆案例）。
// 其他扩展请用搜索框尝试，多数会因 "未提供 Windows 安装包" 失败 —— 这是上游生态局限，不是 NaxOne bug。
const recommendedExts: RecommendedExt[] = [
  { name: "xdebug/xdebug", ext: "xdebug", display: "Xdebug", description: "断点调试 + 性能分析", category: "调试", minPhp: "8.0", maxPhp: "8.6" },
];

function parsePhpVer(v: string): [number, number, number] {
  const parts = v.split(".").map((n) => parseInt(n) || 0);
  return [parts[0] || 0, parts[1] || 0, parts[2] || 0];
}
function cmpVer(a: [number, number, number], b: [number, number, number]): number {
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}
function compatHint(r: RecommendedExt): { ok: boolean; hint: string } {
  if (!selectedPhp.value) return { ok: true, hint: "" };
  const cur = parsePhpVer(selectedPhp.value.version);
  const min = parsePhpVer(r.minPhp);
  if (cmpVer(cur, min) < 0) {
    return { ok: false, hint: `需要 PHP ≥ ${r.minPhp}` };
  }
  if (r.maxPhp) {
    const max = parsePhpVer(r.maxPhp);
    if (cmpVer(cur, max) >= 0) {
      return { ok: false, hint: `需要 PHP < ${r.maxPhp}` };
    }
  }
  return { ok: true, hint: "" };
}

function isExtInstalled(extName: string): boolean {
  const target = extName.toLowerCase();
  return phpExts.value.some(e => e.name.toLowerCase() === target);
}

const showPieModal = ref(false);
const pieRuntime = ref<PieRuntimeInfo | null>(null);
const pieKw = ref("");
const pieInstallLog = ref<string[]>([]);
const pieLogEl = ref<HTMLElement | null>(null);
let pieLogUnlisten: UnlistenFn | null = null;
watch(pieInstallLog, async () => {
  await nextTick();
  if (pieLogEl.value) pieLogEl.value.scrollTop = pieLogEl.value.scrollHeight;
}, { deep: true });
const pieResults = ref<PieExtensionInfo[]>([]);
const pieSearching = ref(false);
const pieInstallingPkg = ref<string>("");

const showingRecommended = computed(() => pieKw.value.trim().length === 0);

async function openPieInstall() {
  if (!selectedPhp.value) { showError("请先选择 PHP 版本"); return; }
  pieKw.value = "";
  pieResults.value = [];
  showPieModal.value = true;
  try { pieRuntime.value = await invoke<PieRuntimeInfo>("pie_runtime_info"); }
  catch (e) { showError("PIE 状态查询失败: " + e); }
}

async function runPieSearch() {
  if (!pieKw.value.trim()) { pieResults.value = []; return; }
  pieSearching.value = true;
  try { pieResults.value = await invoke<PieExtensionInfo[]>("pie_search", { keyword: pieKw.value.trim() }); }
  catch (e) { showError("搜索失败: " + e); pieResults.value = []; }
  finally { pieSearching.value = false; }
}

async function copyPieLog() {
  const text = pieInstallLog.value.join("\n");
  try {
    await navigator.clipboard.writeText(text);
    toast.success("已复制安装日志");
  } catch {
    showError("复制失败");
  }
}

async function doPieInstall(pkg: string) {
  if (!selectedPhp.value) return;
  pieInstallingPkg.value = pkg;
  pieInstallLog.value = [`▶ 开始安装 ${pkg} 到 ${selectedPhp.value.label}…`];
  // 监听后端 pie-install-log event
  if (pieLogUnlisten) { pieLogUnlisten(); pieLogUnlisten = null; }
  pieLogUnlisten = await listen<string>("pie-install-log", (e) => {
    // PIE 输出含 ANSI 颜色码（\x1b[33m...），直接显示是乱码 → strip
    const clean = e.payload.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, "");
    pieInstallLog.value.push(clean);
  });
  try {
    await invoke("pie_install", { package: pkg, targetPhpInstallPath: selectedPhp.value.install_path });
    pieInstallLog.value.push(`✓ 安装完成`);
    toast.success(`扩展 ${pkg} 已安装。重启 PHP 后生效。`);
    await loadPhpExts();
    // 给用户一个查看日志的机会，不立即关闭
    setTimeout(() => { showPieModal.value = false; }, 1500);
  } catch (e) {
    pieInstallLog.value.push(`✗ 失败: ${e}`);
    showError("安装失败: " + e);
  } finally {
    pieInstallingPkg.value = "";
    if (pieLogUnlisten) { pieLogUnlisten(); pieLogUnlisten = null; }
  }
}
const phpInstanceOptions = computed(() => phpInstances.value.map((inst) => ({ label: inst.label, value: inst.id })));
function selectPhpInstance(id: string | number | boolean | null) {
  selectedPhp.value = phpInstances.value.find(i => i.id === id) || null;
}

// ======================== Hosts ========================
const hostsText = ref("");
const hostsOriginal = ref("");
const hostsPath = ref("");

function hostsDirty(): boolean {
  return hostsText.value !== hostsOriginal.value;
}

async function loadHosts() {
  try {
    hostsPath.value = await invoke<string>("get_hosts_file_path");
    const text = await invoke<string>("get_hosts_text");
    hostsText.value = text;
    hostsOriginal.value = text;
  } catch (e) {
    showError("加载 hosts 失败: " + e);
  }
}

async function saveHosts() {
  if (busy.value) return;
  busy.value = true;
  try {
    await invoke("save_hosts_text", { text: hostsText.value });
    hostsOriginal.value = hostsText.value;
    showSaved();
  } catch (e) {
    const msg = String(e ?? "");
    if (msg.startsWith("PERMISSION_DENIED:")) {
      const ok = await confirm("保存 hosts 需要管理员权限，是否继续提权保存？");
      if (!ok) return;
      await invoke("save_hosts_text_elevated", { text: hostsText.value });
      hostsOriginal.value = hostsText.value;
      showSaved();
    } else {
      showError("保存 hosts 失败: " + e);
    }
  } finally {
    busy.value = false;
  }
}

async function openHostsExternally() {
  try {
    const path = hostsPath.value || await invoke<string>("get_hosts_file_path");
    await invoke("open_file", { path });
  } catch (e) {
    showError("打开 hosts 失败: " + e);
  }
}
async function openConfigFile() {
  try {
    if (activeTab.value === "php" && selectedPhp.value) {
      await invoke("open_file", { path: selectedPhp.value.install_path + "/php.ini" });
    } else if (activeTab.value === "hosts") {
      const path = hostsPath.value || await invoke<string>("get_hosts_file_path");
      await invoke("open_file", { path });
    } else {
      const path = await invoke<string>("get_config_file_path", { service: activeTab.value });
      await invoke("open_file", { path });
    }
  } catch (e) { showError("打开失败: " + e); }
}
const logContent = ref("");
const showLog = ref(false);

async function viewLog() {
  try {
    logContent.value = await invoke("find_and_read_log", { service: activeTab.value });
    showLog.value = true;
  } catch (e) { showError("读取日志失败: " + e); }
}

function loadTab() {
  if (activeTab.value === "nginx" && !nginx.value) loadNginx();
  if (activeTab.value === "mysql" && !mysql.value) loadMysql();
  if (activeTab.value === "redis" && !redis.value) loadRedis();
  if (activeTab.value === "php" && phpInstances.value.length === 0) { loadPhpInstances(); }
  if (activeTab.value === "hosts" && !hostsPath.value) { loadHosts(); }
  // env tab 自带数据加载（GlobalEnv 组件 onMounted 拉取），这里不用做事
}
watch(activeTab, loadTab);
onMounted(() => {
  // 支持 /config?tab=xxx 直接定位
  const q = route.query.tab;
  if (typeof q === "string" && (["nginx","mysql","redis","php","hosts","env"] as const).includes(q as Tab)) {
    activeTab.value = q as Tab;
  }
  loadTab();
});
</script>

<template>
  <div class="max-w-[960px] has-save-bar">
    <div class="tabs-row">
      <button class="tab tab-env" :class="{ active: activeTab === 'env' }" @click="setTab('env')">全局环境</button>
      <button class="tab" :class="{ active: activeTab === 'nginx' }" @click="setTab('nginx')">Nginx</button>
      <button class="tab" :class="{ active: activeTab === 'mysql' }" @click="setTab('mysql')">MySQL</button>
      <button class="tab" :class="{ active: activeTab === 'redis' }" @click="setTab('redis')">Redis</button>
      <button class="tab" :class="{ active: activeTab === 'php' }" @click="setTab('php')">PHP</button>
      <button class="tab" :class="{ active: activeTab === 'hosts' }" @click="setTab('hosts')">Hosts</button>
    </div>

    <!-- ==================== Nginx ==================== -->
    <div v-if="activeTab === 'nginx' && nginx" class="tab-card p-6">
      <div class="cfg-section">进程与连接</div>
      <div class="cfg-grid">
        <div class="fg"><label>工作进程数 <span class="k">worker_processes</span></label><input class="input" v-model="nginx.worker_processes" /></div>
        <div class="fg"><label>最大连接数 <span class="k">worker_connections</span></label><input class="input" type="number" v-model.number="nginx.worker_connections" /></div>
        <div class="fg"><label>文件描述符限制 <span class="k">worker_rlimit_nofile</span></label><input class="input" type="number" v-model.number="nginx.worker_rlimit_nofile" /></div>
        <div class="fg"><label>域名哈希桶大小 <span class="k">server_names_hash_bucket_size</span></label><input class="input" type="number" v-model.number="nginx.server_names_hash_bucket_size" /></div>
      </div>
      <div class="cfg-section">超时与保活</div>
      <div class="cfg-grid">
        <div class="fg"><label>保活超时(秒) <span class="k">keepalive_timeout</span></label><input class="input" type="number" v-model.number="nginx.keepalive_timeout" /></div>
        <div class="fg"><label>单连接最大请求数 <span class="k">keepalive_requests</span></label><input class="input" type="number" v-model.number="nginx.keepalive_requests" /></div>
        <div class="fg"><label>发送超时(秒) <span class="k">send_timeout</span></label><input class="input" type="number" v-model.number="nginx.send_timeout" /></div>
        <div class="fg"><label>超时连接重置 <span class="k">reset_timedout_connection</span></label><SelectMenu v-model="nginx.reset_timedout_connection" :options="boolOptions" full-width trigger-class="input" /></div>
      </div>
      <div class="cfg-section">文件传输</div>
      <div class="cfg-grid">
        <div class="fg"><label>高效文件传输 <span class="k">sendfile</span></label><SelectMenu v-model="nginx.sendfile" :options="boolOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>单次发送最大字节 <span class="k">sendfile_max_chunk</span></label><input class="input" v-model="nginx.sendfile_max_chunk" /></div>
      </div>
      <div class="cfg-section">请求限制</div>
      <div class="cfg-grid">
        <div class="fg"><label>请求体大小限制 <span class="k">client_max_body_size</span></label><input class="input" v-model="nginx.client_max_body_size" /></div>
        <div class="fg"><label>请求体缓冲区 <span class="k">client_body_buffer_size</span></label><input class="input" v-model="nginx.client_body_buffer_size" /></div>
        <div class="fg"><label>请求体超时(秒) <span class="k">client_body_timeout</span></label><input class="input" type="number" v-model.number="nginx.client_body_timeout" /></div>
        <div class="fg"><label>请求头缓冲区 <span class="k">client_header_buffer_size</span></label><input class="input" v-model="nginx.client_header_buffer_size" /></div>
        <div class="fg"><label>请求头超时(秒) <span class="k">client_header_timeout</span></label><input class="input" type="number" v-model.number="nginx.client_header_timeout" /></div>
        <div class="fg"><label>大请求头缓冲 <span class="k">large_client_header_buffers</span></label><input class="input" v-model="nginx.large_client_header_buffers" /></div>
      </div>
      <div class="cfg-section">Gzip 压缩</div>
      <div class="cfg-grid">
        <div class="fg"><label>启用 Gzip <span class="k">gzip</span></label><SelectMenu v-model="nginx.gzip" :options="boolOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>压缩等级(1-9) <span class="k">gzip_level</span></label><input class="input" type="number" v-model.number="nginx.gzip_level" min="1" max="9" step="1" /></div>
        <div class="fg"><label>最小压缩体积(字节) <span class="k">gzip_min_length</span></label><input class="input" type="number" v-model.number="nginx.gzip_min_length" min="0" step="1" /></div>
      </div>
    </div>

    <!-- ==================== MySQL ==================== -->
    <div v-if="activeTab === 'mysql' && mysql" class="tab-card p-6">
      <div class="cfg-section">网络</div>
      <div class="cfg-grid">
        <div class="fg"><label>端口 <span class="k">port</span></label><input class="input" type="number" v-model.number="mysql.port" /></div>
        <div class="fg"><label>最大连接数 <span class="k">max_connections</span></label><input class="input" type="number" v-model.number="mysql.max_connections" /></div>
        <div class="fg"><label>交互超时(秒) <span class="k">interactive_timeout</span></label><input class="input" type="number" v-model.number="mysql.interactive_timeout" /></div>
        <div class="fg"><label>等待超时(秒) <span class="k">wait_timeout</span></label><input class="input" type="number" v-model.number="mysql.wait_timeout" /></div>
      </div>
      <div class="cfg-section">InnoDB 引擎</div>
      <div class="cfg-grid">
        <div class="fg"><label>缓冲池大小 <span class="k">innodb_buffer_pool_size</span></label><input class="input" v-model="mysql.innodb_buffer_pool_size" /></div>
        <div class="fg"><label>日志文件大小 <span class="k">innodb_log_file_size</span></label><input class="input" v-model="mysql.innodb_log_file_size" /></div>
        <div class="fg"><label>日志缓冲大小 <span class="k">innodb_log_buffer_size</span></label><input class="input" v-model="mysql.innodb_log_buffer_size" /></div>
        <div class="fg"><label>事务刷新策略 <span class="k">innodb_flush_log_at_trx_commit</span></label><SelectMenu v-model="mysql.innodb_flush_log_at_trx_commit" :options="mysqlFlushOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>锁等待超时(秒) <span class="k">innodb_lock_wait_timeout</span></label><input class="input" type="number" v-model.number="mysql.innodb_lock_wait_timeout" /></div>
      </div>
      <div class="cfg-section">缓冲与缓存</div>
      <div class="cfg-grid">
        <div class="fg"><label>最大数据包 <span class="k">max_allowed_packet</span></label><input class="input" v-model="mysql.max_allowed_packet" /></div>
        <div class="fg"><label>键缓冲区 <span class="k">key_buffer_size</span></label><input class="input" v-model="mysql.key_buffer_size" /></div>
        <div class="fg"><label>表打开缓存 <span class="k">table_open_cache</span></label><input class="input" type="number" v-model.number="mysql.table_open_cache" /></div>
        <div class="fg"><label>临时表大小 <span class="k">tmp_table_size</span></label><input class="input" v-model="mysql.tmp_table_size" /></div>
        <div class="fg"><label>内存表最大大小 <span class="k">max_heap_table_size</span></label><input class="input" v-model="mysql.max_heap_table_size" /></div>
        <div class="fg"><label>排序缓冲区 <span class="k">sort_buffer_size</span></label><input class="input" v-model="mysql.sort_buffer_size" /></div>
        <div class="fg"><label>读缓冲区 <span class="k">read_buffer_size</span></label><input class="input" v-model="mysql.read_buffer_size" /></div>
        <div class="fg"><label>随机读缓冲区 <span class="k">read_rnd_buffer_size</span></label><input class="input" v-model="mysql.read_rnd_buffer_size" /></div>
        <div class="fg"><label>JOIN 缓冲区 <span class="k">join_buffer_size</span></label><input class="input" v-model="mysql.join_buffer_size" /></div>
        <div class="fg"><label>MyISAM 排序缓冲 <span class="k">myisam_sort_buffer_size</span></label><input class="input" v-model="mysql.myisam_sort_buffer_size" /></div>
        <div class="fg"><label>线程缓存 <span class="k">thread_cache_size</span></label><input class="input" type="number" v-model.number="mysql.thread_cache_size" /></div>
      </div>
      <div class="cfg-section">字符集与存储</div>
      <div class="cfg-grid">
        <div class="fg"><label>服务端字符集 <span class="k">character-set-server</span></label><SelectMenu v-model="mysql.character_set_server" :options="mysqlCharsetOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>排序规则 <span class="k">collation-server</span></label><input class="input" v-model="mysql.collation_server" /></div>
        <div class="fg"><label>初始化连接命令 <span class="k">init_connect</span></label><input class="input" v-model="mysql.init_connect" /></div>
        <div class="fg"><label>默认存储引擎 <span class="k">default-storage-engine</span></label><SelectMenu v-model="mysql.default_storage_engine" :options="mysqlEngineOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>错误日志详细级别 <span class="k">log_error_verbosity</span></label><SelectMenu v-model="mysql.log_error_verbosity" :options="mysqlLogVerbosityOptions" full-width trigger-class="input" /></div>
      </div>
    </div>

    <!-- ==================== Redis ==================== -->
    <div v-if="activeTab === 'redis' && redis" class="tab-card p-6">
      <div class="cfg-section">网络</div>
      <div class="cfg-grid">
        <div class="fg"><label>端口 <span class="k">port</span></label><input class="input" type="number" v-model.number="redis.port" /></div>
        <div class="fg"><label>绑定地址 <span class="k">bind</span></label><input class="input" v-model="redis.bind" /></div>
        <div class="fg"><label>客户端超时(秒) <span class="k">timeout</span></label><input class="input" type="number" v-model.number="redis.timeout" /></div>
        <div class="fg"><label>最大客户端数 <span class="k">maxclients</span></label><input class="input" type="number" v-model.number="redis.maxclients" /></div>
        <div class="fg"><label>TCP Backlog <span class="k">tcp-backlog</span></label><input class="input" type="number" v-model.number="redis.tcp_backlog" /></div>
        <div class="fg"><label>TCP Keepalive <span class="k">tcp-keepalive</span></label><input class="input" type="number" v-model.number="redis.tcp_keepalive" /></div>
      </div>
      <div class="cfg-section">内存</div>
      <div class="cfg-grid">
        <div class="fg"><label>最大内存(字节) <span class="k">maxmemory</span></label><input class="input" v-model="redis.maxmemory" /></div>
        <div class="fg"><label>淘汰策略 <span class="k">maxmemory-policy</span></label><SelectMenu v-model="redis.maxmemory_policy" :options="redisPolicyOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>数据库数量 <span class="k">databases</span></label><input class="input" type="number" v-model.number="redis.databases" /></div>
      </div>
      <div class="cfg-section">持久化</div>
      <div class="cfg-grid">
        <div class="fg"><label>RDB 压缩 <span class="k">rdbcompression</span></label><SelectMenu v-model="redis.rdbcompression" :options="boolOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>RDB 校验和 <span class="k">rdbchecksum</span></label><SelectMenu v-model="redis.rdbchecksum" :options="boolOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>RDB 文件名 <span class="k">dbfilename</span></label><input class="input" v-model="redis.dbfilename" /></div>
        <div class="fg"><label>工作目录 <span class="k">dir</span></label><input class="input" v-model="redis.dir" /></div>
        <div class="fg"><label>保存失败时停止写入 <span class="k">stop-writes-on-bgsave-error</span></label><SelectMenu v-model="redis.stop_writes_on_bgsave_error" :options="boolOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>AOF 持久化 <span class="k">appendonly</span></label><SelectMenu v-model="redis.appendonly" :options="boolOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>AOF 刷盘策略 <span class="k">appendfsync</span></label><SelectMenu v-model="redis.appendfsync" :options="redisAppendfsyncOptions" full-width trigger-class="input" /></div>
      </div>
      <div class="cfg-section">安全</div>
      <div class="cfg-grid">
        <div class="fg full"><label>访问密码 <span class="k">requirepass</span> <span class="text-[13px] text-content-muted font-normal">留空表示无密码</span></label><input class="input" v-model="redis.requirepass" placeholder="留空则不设密码" /></div>
      </div>
      <div class="cfg-section">日志</div>
      <div class="cfg-grid">
        <div class="fg"><label>日志级别 <span class="k">loglevel</span></label><SelectMenu v-model="redis.loglevel" :options="redisLoglevelOptions" full-width trigger-class="input" /></div>
        <div class="fg"><label>日志文件 <span class="k">logfile</span></label><input class="input" v-model="redis.logfile" placeholder="留空输出到控制台" /></div>
      </div>
    </div>

    <!-- ==================== PHP ==================== -->
    <div v-if="activeTab === 'php'" class="tab-card p-6">
      <!-- PHP Version Selector -->
      <div class="flex items-center justify-between mb-5 gap-4">
        <SelectMenu v-if="phpInstances.length > 0" :model-value="selectedPhp?.id ?? null" :options="phpInstanceOptions" trigger-class="input w-[240px] shrink-0" @update:modelValue="selectPhpInstance($event)" />
        <div class="flex gap-1 shrink-0">
          <button
            v-for="st in ([{k:'extensions',l:'扩展管理'},{k:'settings',l:'php.ini 配置'},{k:'phpinfo',l:'phpinfo'}] as const)" :key="st.k"
            class="px-4 py-1.5 rounded-md text-[16px] cursor-pointer transition-all border"
            :class="phpSubTab === st.k ? 'bg-accent-blue text-white border-accent-blue' : 'bg-surface-primary text-content-muted border-border hover:text-content-secondary'"
            @click="switchPhpSubTab(st.k)"
          >{{ st.l }}</button>
        </div>
      </div>

      <!-- Extensions -->
      <div v-if="phpSubTab === 'extensions'">
        <div class="flex items-center justify-between py-2.5 text-[16px] text-content-muted border-b border-border mb-1.5">
          <span>切换开关启用/禁用扩展，修改后需重启 PHP 生效</span>
          <button class="btn btn-primary btn-sm flex items-center gap-1" @click="openPieInstall">
            <Plus :size="14" /> 安装新扩展
          </button>
        </div>
        <div v-for="ext in phpExts" :key="ext.file_name" class="flex items-center gap-3 py-2.5 border-b border-border last:border-b-0 hover:bg-surface-hover -mx-2 px-2 rounded transition-colors">
          <label class="toggle-wrap"><input type="checkbox" :checked="ext.enabled" :disabled="busy" @change="toggleExt(ext)" /><span class="toggle-slider"></span></label>
          <span class="text-[16px]">{{ ext.name }}</span>
          <span v-if="ext.is_zend" class="text-2xs px-2 py-0.5 rounded bg-[#3730a3] text-[#c7d2fe]">Zend</span>
        </div>
      </div>

      <!-- INI Settings -->
      <div v-if="phpSubTab === 'settings' && phpIni">
        <div class="cfg-section">基础配置</div>
        <div class="cfg-grid">
          <div class="fg"><label>内存限制 <span class="k">memory_limit</span></label><input class="input" v-model="phpIni.memory_limit" /></div>
          <div class="fg"><label>上传文件大小限制 <span class="k">upload_max_filesize</span></label><input class="input" v-model="phpIni.upload_max_filesize" /></div>
          <div class="fg"><label>POST 数据大小限制 <span class="k">post_max_size</span></label><input class="input" v-model="phpIni.post_max_size" /></div>
          <div class="fg"><label>最大执行时间(秒) <span class="k">max_execution_time</span></label><input class="input" type="number" v-model.number="phpIni.max_execution_time" /></div>
          <div class="fg"><label>输入超时(秒) <span class="k">max_input_time</span></label><input class="input" type="number" v-model.number="phpIni.max_input_time" /></div>
          <div class="fg"><label>时区 <span class="k">date.timezone</span></label><input class="input" v-model="phpIni.date_timezone" /></div>
          <div class="fg"><label>错误报告级别 <span class="k">error_reporting</span></label><input class="input" v-model="phpIni.error_reporting" /></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">显示错误 <span class="k">display_errors</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.display_errors" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">短标签 <span class="k">short_open_tag</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.short_open_tag" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">文件上传 <span class="k">file_uploads</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.file_uploads" /><span class="toggle-slider"></span></label></div>
        </div>
        <div class="cfg-section">安全</div>
        <div class="cfg-grid">
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">允许远程文件 <span class="k">allow_url_fopen</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.allow_url_fopen" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">允许远程包含 <span class="k">allow_url_include</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.allow_url_include" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">暴露 PHP 版本 <span class="k">expose_php</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.expose_php" /><span class="toggle-slider"></span></label></div>
          <div class="fg full"><label>禁用函数 <span class="k">disable_functions</span></label><input class="input" v-model="phpIni.disable_functions" placeholder="exec,passthru,shell_exec,system" /></div>
          <div class="fg full"><label>目录限制 <span class="k">open_basedir</span></label><input class="input" v-model="phpIni.open_basedir" placeholder="留空不限制" /></div>
        </div>
        <div class="cfg-section">OPCache 缓存</div>
        <div class="cfg-grid">
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">启用 OPCache <span class="k">opcache.enable</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.opcache_enable" /><span class="toggle-slider"></span></label></div>
          <div class="fg"><label>内存消耗(MB) <span class="k">opcache.memory_consumption</span></label><input class="input" type="number" v-model.number="phpIni.opcache_memory_consumption" /></div>
          <div class="fg"><label>最大加速文件数 <span class="k">opcache.max_accelerated_files</span></label><input class="input" type="number" v-model.number="phpIni.opcache_max_accelerated_files" /></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">验证时间戳 <span class="k">opcache.validate_timestamps</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.opcache_validate_timestamps" /><span class="toggle-slider"></span></label></div>
          <div class="fg"><label>重验证间隔(秒) <span class="k">opcache.revalidate_freq</span></label><input class="input" type="number" v-model.number="phpIni.opcache_revalidate_freq" /></div>
        </div>
        <div class="cfg-section">输出与编码</div>
        <div class="cfg-grid">
          <div class="fg"><label>输出缓冲 <span class="k">output_buffering</span></label><input class="input" v-model="phpIni.output_buffering" /></div>
          <div class="fg"><label>默认字符集 <span class="k">default_charset</span></label><input class="input" v-model="phpIni.default_charset" /></div>
          <div class="fg"><label>最大上传文件数 <span class="k">max_file_uploads</span></label><input class="input" type="number" v-model.number="phpIni.max_file_uploads" /></div>
          <div class="fg"><label>Socket 超时(秒) <span class="k">default_socket_timeout</span></label><input class="input" type="number" v-model.number="phpIni.default_socket_timeout" /></div>
        </div>
        <div class="cfg-section">Session 会话</div>
        <div class="cfg-grid">
          <div class="fg"><label>存储方式 <span class="k">session.save_handler</span></label><SelectMenu v-model="phpIni.session_save_handler" :options="phpSessionHandlerOptions" full-width trigger-class="input" /></div>
          <div class="fg"><label>存储路径 <span class="k">session.save_path</span></label><input class="input" v-model="phpIni.session_save_path" placeholder="默认系统临时目录" /></div>
          <div class="fg"><label>Session 名称 <span class="k">session.name</span></label><input class="input" v-model="phpIni.session_name" /></div>
          <div class="fg"><label>GC 最大存活时间(秒) <span class="k">session.gc_maxlifetime</span></label><input class="input" type="number" v-model.number="phpIni.session_gc_maxlifetime" /></div>
          <div class="fg"><label>Cookie 有效期(秒) <span class="k">session.cookie_lifetime</span></label><input class="input" type="number" v-model.number="phpIni.session_cookie_lifetime" /></div>
          <div class="fg"><label>Cookie SameSite <span class="k">session.cookie_samesite</span></label><SelectMenu v-model="phpIni.session_cookie_samesite" :options="phpSameSiteOptions" full-width trigger-class="input" /></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">使用 Cookie <span class="k">session.use_cookies</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.session_use_cookies" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">仅使用 Cookie <span class="k">session.use_only_cookies</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.session_use_only_cookies" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">严格模式 <span class="k">session.use_strict_mode</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.session_use_strict_mode" /><span class="toggle-slider"></span></label></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">Cookie HttpOnly <span class="k">session.cookie_httponly</span></span><label class="toggle-wrap"><input type="checkbox" v-model="phpIni.session_cookie_httponly" /><span class="toggle-slider"></span></label></div>
        </div>
      </div>

      <!-- phpinfo -->
      <div v-if="phpSubTab === 'phpinfo'">
        <div class="flex items-center gap-2 mb-3">
          <div class="flex-1 text-[12px]" style="color: var(--text-muted)">
            源：<code>{{ selectedPhp?.install_path || '' }}\php-cgi.exe -q -i</code>
          </div>
          <button class="btn btn-secondary btn-sm" :disabled="!phpinfoFileUrl" @click="openPhpInfoInBrowser">
            在浏览器打开
          </button>
          <button class="btn btn-secondary btn-sm" :disabled="phpinfoLoading" @click="loadPhpInfo">
            {{ phpinfoLoading ? '加载中...' : '刷新' }}
          </button>
        </div>

        <div v-if="phpinfoLoading" class="text-[12px] p-3" style="color: var(--text-muted)">正在跑 php -r "phpinfo();" ...</div>
        <div v-else-if="phpinfoError" class="text-[12px] p-3" style="color: var(--danger)">{{ phpinfoError }}</div>
        <iframe
          v-else-if="phpinfoHtml"
          class="w-full"
          style="height: 72vh; border: 1px solid var(--border); border-radius: 6px; background: #fff"
          sandbox="allow-same-origin"
          :srcdoc="phpinfoHtml"
        ></iframe>
      </div>
    </div>


    <!-- ==================== Hosts ==================== -->
    <div v-if="activeTab === 'hosts'" class="tab-card p-6">
      <div class="flex items-center justify-between mb-2">
        <div class="text-[16px] font-medium text-content-secondary">系统 Hosts 文件</div>
        <button class="btn btn-secondary btn-sm" @click="openHostsExternally">系统编辑器打开</button>
      </div>
      <div class="text-[13px] text-content-muted mb-3 break-all">{{ hostsPath || '加载中...' }}</div>
      <textarea
        class="input font-mono text-[13px] leading-5 w-full min-h-[420px]"
        style="resize: vertical"
        v-model="hostsText"
        spellcheck="false"
        @keydown="onTextareaTab"
        placeholder="# 在这里编辑 hosts 内容"
      ></textarea>
    </div>

    <!-- ==================== 全局环境 ==================== -->
    <!-- v-show 而非 v-if：进入 /config 即挂载 GlobalEnv 在后台拉数据，
         切到该 tab 时无网络等待。 -->
    <div v-show="activeTab === 'env'" class="tab-card p-6">
      <GlobalEnv />
    </div>

    <!-- Unified save bar （env tab 自带操作按钮，不需要统一保存条） -->
    <div v-if="activeTab !== 'env'" class="save-bar">
      <button v-if="(activeTab === 'nginx' && nginx) || (activeTab === 'mysql' && mysql) || (activeTab === 'redis' && redis) || (activeTab === 'php' && phpSubTab === 'settings' && phpIni) || activeTab === 'hosts'"
              class="btn btn-success btn-sm" :disabled="busy || (activeTab === 'hosts' && !hostsDirty())"
              @click="activeTab === 'nginx' ? saveNginx() : activeTab === 'mysql' ? saveMysql() : activeTab === 'redis' ? saveRedis() : activeTab === 'php' ? savePhpIni() : saveHosts()">
        {{ busy ? "保存中..." : "保存配置" }}
      </button>
      <button class="btn btn-secondary btn-sm" @click="openConfigFile">打开配置文件</button>
      <button v-if="activeTab === 'nginx' || activeTab === 'mysql'" class="btn btn-secondary btn-sm" @click="viewLog">查看日志</button>
      <span v-if="activeTab === 'hosts' && hostsDirty()" class="saved-msg" style="color: var(--color-warn, #f59e0b)">hosts 有未保存修改</span>
      <span v-else-if="saved" class="saved-msg">已保存，重启服务后生效</span>
    </div>

    <!-- Log modal -->
    <div v-if="showLog" class="modal-overlay">
      <div class="modal-content" style="width: 800px">
        <div class="flex items-center justify-between mb-3">
          <div class="text-base font-semibold">错误日志</div>
          <button class="btn btn-secondary btn-sm" @click="showLog = false">关闭</button>
        </div>
        <pre class="input font-mono text-[13px] whitespace-pre-wrap overflow-y-auto" style="max-height: 60vh; min-height: 200px">{{ logContent || '(日志为空)' }}</pre>
      </div>
    </div>

    <!-- PIE Install Modal -->
    <div v-if="showPieModal" class="modal-overlay" @click.self="showPieModal = false">
      <div class="modal-content" style="width: 720px">
        <div class="flex items-center justify-between mb-3">
          <div class="text-base font-semibold">通过 PIE 安装 PHP 扩展</div>
          <button class="btn btn-secondary btn-sm" @click="showPieModal = false">关闭</button>
        </div>

        <div v-if="selectedPhp" class="text-[13px] mb-2" style="color: var(--text-secondary)">
          目标 PHP：<b>{{ selectedPhp.label }}</b>
          <span style="color: var(--text-muted)">（{{ selectedPhp.install_path }}）</span>
        </div>

        <div v-if="pieRuntime && !pieRuntime.runtime_php_path"
             class="text-[13px] p-3 rounded mb-3"
             style="background: var(--bg-tertiary); border: 1px solid var(--color-danger); color: var(--text-primary)">
          ⚠️ 未找到 PHP 8.1 或更高版本，PIE 无法运行。请先到「软件商店」安装 PHP ≥ 8.1。
          PIE 需要高版本 PHP 当 runtime，但可以装扩展到任意 PHP 版本（包括你当前选的）。
        </div>
        <div v-else-if="pieRuntime" class="text-[12px] mb-3" style="color: var(--text-muted)">
          PIE runtime：PHP {{ pieRuntime.runtime_version }}
        </div>

        <div class="flex gap-2 mb-3">
          <input class="input flex-1" v-model="pieKw" placeholder="搜索其他扩展（清空显示精选列表）"
                 @keyup.enter="runPieSearch" :disabled="!pieRuntime?.runtime_php_path" />
          <button class="btn btn-primary btn-sm flex items-center gap-1"
                  :disabled="pieSearching || !pieRuntime?.runtime_php_path"
                  @click="runPieSearch">
            <Search :size="14" />
            {{ pieSearching ? '搜索中…' : '搜索' }}
          </button>
        </div>

        <!-- 精选列表（默认） -->
        <div v-if="showingRecommended && pieRuntime?.runtime_php_path" class="overflow-y-auto" style="max-height: 50vh">
          <div class="text-[12px] mb-2" style="color: var(--text-muted)">精选扩展</div>
          <div v-for="r in recommendedExts" :key="r.name"
               class="flex items-center gap-2 py-2.5 border-b border-border">
            <div class="flex-1 min-w-0">
              <div class="flex items-center gap-2">
                <span class="text-[14px] font-medium" style="color: var(--text-primary)">{{ r.display }}</span>
                <span class="text-[11px] px-1.5 py-px rounded"
                      style="background: var(--bg-tertiary); color: var(--text-muted); border: 1px solid var(--border-color)">{{ r.category }}</span>
              </div>
              <div class="text-[12px] mt-0.5" style="color: var(--text-muted)">{{ r.description }}</div>
              <div class="text-[11px] mt-0.5" style="color: var(--text-muted); font-family: var(--font-mono)">{{ r.name }}</div>
            </div>
            <span v-if="isExtInstalled(r.ext)"
                  class="text-[12px] px-2 py-1 rounded shrink-0"
                  style="background: var(--bg-tertiary); color: var(--color-success); border: 1px solid var(--color-success)">
              ✓ 已安装
            </span>
            <span v-else-if="!compatHint(r).ok"
                  class="text-[12px] px-2 py-1 rounded shrink-0"
                  style="background: var(--bg-tertiary); color: var(--text-muted); border: 1px solid var(--border-color)"
                  :title="compatHint(r).hint">
              ✗ {{ compatHint(r).hint }}
            </span>
            <button v-else
                    class="btn btn-success btn-sm flex items-center gap-1 shrink-0"
                    :disabled="!!pieInstallingPkg"
                    @click="doPieInstall(r.name)">
              <Download :size="12" />
              {{ pieInstallingPkg === r.name ? '安装中…' : '安装' }}
            </button>
          </div>
        </div>

        <!-- 搜索结果 -->
        <div v-else-if="!showingRecommended && pieResults.length > 0" class="overflow-y-auto" style="max-height: 50vh">
          <div class="text-[12px] mb-2" style="color: var(--text-muted)">
            搜索结果 · 注意：非精选扩展可能不支持 Windows，安装失败属正常
          </div>
          <div v-for="r in pieResults" :key="r.name"
               class="flex items-start gap-2 py-2.5 border-b border-border">
            <div class="flex-1 min-w-0">
              <div class="text-[14px] font-medium" style="color: var(--text-primary); font-family: var(--font-mono)">{{ r.name }}</div>
              <div v-if="r.description" class="text-[12px] mt-1" style="color: var(--text-muted); word-break: break-word">{{ r.description }}</div>
            </div>
            <button class="btn btn-success btn-sm flex items-center gap-1 shrink-0"
                    :disabled="!!pieInstallingPkg"
                    @click="doPieInstall(r.name)">
              <Download :size="12" />
              {{ pieInstallingPkg === r.name ? '安装中…' : '安装' }}
            </button>
          </div>
        </div>
        <div v-else-if="!showingRecommended && !pieSearching && pieRuntime?.runtime_php_path"
             class="text-center py-8 text-[13px]" style="color: var(--text-muted)">
          没有匹配的扩展
        </div>

        <!-- 实时日志（安装中或刚装完） -->
        <div v-if="pieInstallLog.length > 0" class="mt-3">
          <div class="flex items-center justify-between mb-1">
            <span class="text-[12px]" style="color: var(--text-muted)">安装日志</span>
            <button class="btn btn-secondary btn-sm flex items-center gap-1 !px-2 !py-0.5"
                    @click="copyPieLog" title="复制全部日志">
              <Copy :size="12" /> 复制全部
            </button>
          </div>
          <pre class="font-mono text-[12px] p-2.5 rounded overflow-y-auto"
               ref="pieLogEl"
               style="background: var(--bg-input); color: var(--text-primary); border: 1px solid var(--border-color); max-height: 240px; line-height: 1.5; white-space: pre-wrap; word-break: break-all; user-select: text; -webkit-user-select: text; cursor: text">{{ pieInstallLog.join('\n') }}</pre>
        </div>
      </div>
    </div>
  </div>
</template>
