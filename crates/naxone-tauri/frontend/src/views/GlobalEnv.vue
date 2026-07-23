<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { confirm } from "@tauri-apps/plugin-dialog";
import { listen, emit, type UnlistenFn } from "@tauri-apps/api/event";
import { AlertTriangle, Eye, EyeOff } from "lucide-vue-next";
import SelectMenu from "../components/SelectMenu.vue";
import { toast, friendlyError } from "../composables/useToast";

interface ServiceInfo {
  id: string; kind: string; display_name: string; version: string;
  variant: string | null; port: number;
  status: { state: string; pid?: number; memory_mb?: number };
  install_path: string; origin: string;
}

interface GlobalPhpInfo {
  version: string | null;
  bin_dir: string;
  path_registered: boolean;
  conflicts: string[];
}

interface ComposerOption { version: string; source: string; phar_path: string; }
interface ComposerToolInfo { active_version: string | null; available: ComposerOption[]; }
interface NvmToolInfo { nvm_version: string; nvm_source: string; nvm_home: string; current_node: string | null; installed_nodes: string[]; }
interface NodeVersionCatalog { current: string[]; lts: string[]; }
interface MysqlOption { version: string; install_path: string; data_dir: string; bin_dir: string; port: number; initialized: boolean; root_password: string; source: string; }
interface MysqlToolInfo { active_version: string | null; available: MysqlOption[]; conflicts: string[]; }
interface DevToolsInfo { composer: ComposerToolInfo | null; nvm: NvmToolInfo | null; mysql: MysqlToolInfo | null; }

const services = ref<ServiceInfo[]>([]);
const globalPhp = ref<GlobalPhpInfo>({ version: null, bin_dir: "", path_registered: false, conflicts: [] });
const showConflictHelp = ref(false);
const conflictFixBusy = ref(false);
const globalPhpPick = ref("");
const globalPhpBusy = ref(false);

// 首屏加载完成才显示卡片，避免数据未到时闪现"未发现 X"
const loaded = ref(false);

const devTools = ref<DevToolsInfo>({ composer: null, nvm: null, mysql: null });
const nodePick = ref("");
const composerPick = ref("");
const mysqlPick = ref("");

// Composer 镜像源选项：value 为空串 = 官方源；"custom" = 用户填 URL。
const composerMirrors = [
  { label: "Packagist 官方", value: "" },
  { label: "阿里云", value: "https://mirrors.aliyun.com/composer/" },
  { label: "腾讯云", value: "https://mirrors.cloud.tencent.com/composer/" },
  { label: "华为云", value: "https://mirrors.huaweicloud.com/repository/php/" },
  { label: "自定义 URL", value: "custom" },
];
const composerRepoUrl = ref("");        // 后端读到的当前 URL，空串 = 官方
const composerMirrorPick = ref("");     // 下拉当前选中的 value
const composerCustomUrl = ref("");      // pick=custom 时填入的 URL
const composerRepoBusy = ref(false);
const mysqlPwdInput = ref("");
const mysqlCurrentPwd = ref("");
const showMysqlPwd = ref(false);
const mysqlConflictFixBusy = ref(false);
const nodeSwitchBusy = ref(false);
const nodeCatalog = ref<NodeVersionCatalog>({ current: [], lts: [] });
const nodeCatalogBusy = ref(false);
const nodeCatalogLoaded = ref(false);
const nodeInstallPick = ref("");
const nodeCustomVersion = ref("");
const nodeActivateAfterInstall = ref(true);
const nodeInstallBusy = ref(false);
const nodeUninstallBusy = ref(false);
const composerSwitchBusy = ref(false);
const mysqlSwitchBusy = ref(false);
const mysqlPwdBusy = ref(false);

let unlistenServicesChanged: UnlistenFn | null = null;

const phpServices = computed(() => services.value.filter(s => s.kind === "php"));

const currentMysqlOption = computed<MysqlOption | undefined>(() =>
  devTools.value.mysql?.available.find(a => a.version === mysqlPick.value)
);

const nodeDownloadOptions = computed(() => {
  const installed = new Set(devTools.value.nvm?.installed_nodes ?? []);
  const options: Array<{ label: string; value: string }> = [];
  const seen = new Set<string>();
  for (const version of nodeCatalog.value.lts) {
    if (!installed.has(version) && !seen.has(version)) {
      options.push({ label: `v${version} · LTS`, value: version });
      seen.add(version);
    }
  }
  for (const version of nodeCatalog.value.current) {
    if (!installed.has(version) && !seen.has(version)) {
      options.push({ label: `v${version} · Current`, value: version });
      seen.add(version);
    }
  }
  return options;
});

// 切换 MySQL 实例时把已存的 root 密码回填到输入框（默认 type=password 显示为 ***）
watch(currentMysqlOption, (opt) => {
  mysqlPwdInput.value = opt?.root_password || "";
}, { immediate: true });

function originShort(s: ServiceInfo): string {
  if (s.origin === "phpstudy") return "PS";
  if (s.origin === "manual") return "独立";
  if (s.origin === "system") return "系统";
  return "NX";
}
function phpOptionLabel(p: ServiceInfo): string {
  return `v${p.version}${p.variant ? ` ${p.variant}` : ""} [${originShort(p)}]`;
}
function sourceLabel(s: string): string {
  return s === "system" ? "系统" : "NX";
}
function showError(msg: unknown) { toast.error(friendlyError(msg)); }

async function loadServices() {
  try { services.value = await invoke("get_services"); }
  catch (e) { showError(`加载服务失败: ${e}`); }
}

async function loadGlobalPhp() {
  try {
    globalPhp.value = await invoke("get_global_php_version");
    if (!globalPhpPick.value) {
      globalPhpPick.value = globalPhp.value.version ?? phpServices.value[0]?.version ?? "";
    }
  } catch (e) { showError(`读取全局 PHP 失败: ${e}`); }
}

async function loadDevTools() {
  try {
    devTools.value = await invoke("get_dev_tools_info");
    if (devTools.value.nvm?.current_node && !nodePick.value) {
      nodePick.value = devTools.value.nvm.current_node;
    }
    if (devTools.value.composer?.active_version && !composerPick.value) {
      composerPick.value = devTools.value.composer.active_version;
    }
    if (devTools.value.mysql && !mysqlPick.value) {
      mysqlPick.value = devTools.value.mysql.active_version
        ?? devTools.value.mysql.available[0]?.version
        ?? "";
    }
  } catch (e) { showError(`读取开发工具失败: ${e}`); }
}

async function applyGlobalPhp() {
  if (!globalPhpPick.value || globalPhpBusy.value) return;
  const version = globalPhpPick.value;
  const firstTime = !globalPhp.value.version;
  const hint = firstTime
    ? `将 CLI 的 php 命令切到 v${version}。首次使用会把 ~/.naxone/bin 加入用户 PATH。继续？`
    : `切换 CLI php 到 v${version}。新开命令行窗口即可生效。继续？`;
  const ok = await confirm(hint, { title: "设置全局 PHP", kind: "info" });
  if (!ok) return;

  globalPhpBusy.value = true;
  try {
    globalPhp.value = await invoke("set_global_php_version", { version });
    const tip = firstTime && globalPhp.value.path_registered
      ? `全局 PHP 已切到 v${version}。请**新开** cmd 窗口跑 \`php -v\` 验证。`
      : `全局 PHP 已切到 v${version}`;
    toast.success(tip);
    emit("global-env-changed");
  } catch (e) { showError(`设置失败: ${e}`); }
  finally { globalPhpBusy.value = false; }
}

async function fixConflicts() {
  if (conflictFixBusy.value || !globalPhp.value.conflicts.length) return;
  const list = globalPhp.value.conflicts;
  const ok = await confirm(
    `即将从系统 PATH 清除 ${list.length} 条 PHP 路径：\n\n${list.join("\n")}\n\n需要管理员权限（会弹 UAC 确认）。`,
    { title: "一键修复 PATH 冲突", kind: "warning" }
  );
  if (!ok) return;
  conflictFixBusy.value = true;
  try {
    globalPhp.value = await invoke("fix_global_php_conflicts", { paths: list });
    if (globalPhp.value.conflicts.length === 0) {
      toast.success("清理完成。请**新开** cmd 窗口验证。");
    } else {
      toast.warn(`部分路径仍在系统 PATH 中（${globalPhp.value.conflicts.length} 条）`);
    }
  } catch (e) { showError(`修复失败: ${e}`); }
  finally { conflictFixBusy.value = false; }
}

async function openEnvEditor() {
  try { await invoke("open_system_env_editor"); }
  catch (e) { showError(`打开失败: ${e}`); }
}

async function switchNode() {
  if (!nodePick.value || nodeSwitchBusy.value) return;
  nodeSwitchBusy.value = true;
  try {
    const result: NvmToolInfo = await invoke("switch_node_version", { version: nodePick.value });
    devTools.value = { ...devTools.value, nvm: result };
    toast.success(`Node.js 已切到 v${result.current_node ?? nodePick.value}。请新开 cmd 跑 \`node -v\` 验证。`);
    emit("global-env-changed");
  } catch (e) { showError(`切换失败: ${e}`); }
  finally { nodeSwitchBusy.value = false; }
}

async function loadNodeCatalog() {
  if (nodeCatalogBusy.value) return;
  nodeCatalogBusy.value = true;
  try {
    nodeCatalog.value = await invoke("list_available_node_versions");
    nodeCatalogLoaded.value = true;
    if (!nodeInstallPick.value || !nodeDownloadOptions.value.some(o => o.value === nodeInstallPick.value)) {
      nodeInstallPick.value = nodeDownloadOptions.value[0]?.value ?? "";
    }
  } catch (e) { showError(`获取 Node.js 可下载版本失败: ${e}`); }
  finally { nodeCatalogBusy.value = false; }
}

async function installNode() {
  if (nodeInstallBusy.value) return;
  const version = (nodeCustomVersion.value.trim() || nodeInstallPick.value).replace(/^v/i, "");
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    showError("请输入完整的 Node.js 版本号，例如 22.22.3");
    return;
  }

  nodeInstallBusy.value = true;
  try {
    const result: NvmToolInfo = await invoke("install_node_version", {
      version,
      activate: nodeActivateAfterInstall.value,
    });
    devTools.value = { ...devTools.value, nvm: result };
    nodePick.value = nodeActivateAfterInstall.value ? (result.current_node ?? version) : version;
    nodeCustomVersion.value = "";
    nodeInstallPick.value = nodeDownloadOptions.value[0]?.value ?? "";
    const suffix = nodeActivateAfterInstall.value ? "并已设为当前版本" : "";
    toast.success(`Node.js v${version} 已下载安装${suffix}`);
    emit("global-env-changed");
  } catch (e) { showError(`安装失败: ${e}`); }
  finally { nodeInstallBusy.value = false; }
}

async function uninstallNode() {
  const version = nodePick.value;
  if (!version || nodeUninstallBusy.value || version === devTools.value.nvm?.current_node) return;
  const ok = await confirm(
    `确定卸载 Node.js v${version}？该版本目录及其全局 npm 包会被删除。`,
    { title: "卸载 Node.js", kind: "warning" }
  );
  if (!ok) return;

  nodeUninstallBusy.value = true;
  try {
    const result: NvmToolInfo = await invoke("uninstall_node_version", { version });
    devTools.value = { ...devTools.value, nvm: result };
    nodePick.value = result.current_node ?? result.installed_nodes[0] ?? "";
    toast.success(`Node.js v${version} 已卸载`);
    emit("global-env-changed");
  } catch (e) { showError(`卸载失败: ${e}`); }
  finally { nodeUninstallBusy.value = false; }
}

async function loadComposerRepo() {
  // composer 没激活就不读，避免无谓报错
  if (!devTools.value.composer?.active_version) return;
  try {
    const url: string = await invoke("get_composer_repo");
    composerRepoUrl.value = url;
    // 根据当前 URL 推断下拉选项
    const matched = composerMirrors.find(m => m.value && m.value === url);
    if (url === "") {
      composerMirrorPick.value = "";
    } else if (matched) {
      composerMirrorPick.value = matched.value;
    } else {
      composerMirrorPick.value = "custom";
      composerCustomUrl.value = url;
    }
  } catch (e) {
    // 读不到（composer 没装 php 等）不抛 toast，保持空状态
    console.warn("读 composer 镜像源失败:", e);
  }
}

async function applyComposerMirror() {
  if (composerRepoBusy.value) return;
  const pick = composerMirrorPick.value;
  let url = "";
  if (pick === "custom") {
    url = composerCustomUrl.value.trim();
    if (!url) { showError("请填写自定义镜像 URL"); return; }
    if (!/^https?:\/\//i.test(url)) { showError("URL 必须以 http:// 或 https:// 开头"); return; }
  } else {
    url = pick;  // 空串 = 官方
  }
  if (url === composerRepoUrl.value) return;

  composerRepoBusy.value = true;
  try {
    const result: string = await invoke("set_composer_repo", { url });
    composerRepoUrl.value = result;
    toast.success(url === ""
      ? "已重置为 Packagist 官方源"
      : `Composer 镜像源已设为 ${url}`);
    emit("global-env-changed");
  } catch (e) { showError(`设置失败: ${e}`); }
  finally { composerRepoBusy.value = false; }
}

async function switchComposer() {
  if (!composerPick.value || composerSwitchBusy.value) return;
  composerSwitchBusy.value = true;
  try {
    const result: DevToolsInfo = await invoke("set_global_composer", { version: composerPick.value });
    devTools.value = result;
    toast.success(`Composer 已切到 v${composerPick.value}。请新开 cmd 跑 \`composer -V\` 验证。`);
    emit("global-env-changed");
  } catch (e) { showError(`切换失败: ${e}`); }
  finally { composerSwitchBusy.value = false; }
}

async function switchMysql() {
  if (!mysqlPick.value || mysqlSwitchBusy.value) return;
  mysqlSwitchBusy.value = true;
  try {
    const result: DevToolsInfo = await invoke("set_global_mysql", { version: mysqlPick.value });
    devTools.value = result;
    toast.success(`MySQL 已切到 v${mysqlPick.value}。请新开 cmd 跑 \`mysql --version\` 验证。⚠ 不同 MySQL 实例的 root 密码可能不同，请到下方"MySQL 密码"重新查看/设置。`);
    emit("global-env-changed");
  } catch (e) { showError(`切换失败: ${e}`); }
  finally { mysqlSwitchBusy.value = false; }
}

async function saveMysqlPassword() {
  const ver = mysqlPick.value;
  const pwd = mysqlPwdInput.value;
  if (!ver) { showError("请先选择 MySQL 版本"); return; }
  if (!pwd) { showError("新密码不能为空"); return; }

  // 当前密码：UI 上的输入优先；否则用 stored root_password；都空则后端会用空串试一次
  const opt = currentMysqlOption.value;
  const current = mysqlCurrentPwd.value || opt?.root_password || "";

  mysqlPwdBusy.value = true;
  try {
    const result: DevToolsInfo = await invoke("set_mysql_password", {
      version: ver,
      newPassword: pwd,
      currentPassword: current || null,
    });
    devTools.value = result;
    mysqlPwdInput.value = "";
    mysqlCurrentPwd.value = "";
    toast.success(`MySQL v${ver} root 密码已更新`);
  } catch (e) { showError(`修改密码失败: ${e}`); }
  finally { mysqlPwdBusy.value = false; }
}

async function fixMysqlPathConflicts() {
  const list = devTools.value.mysql?.conflicts ?? [];
  if (!list.length || mysqlConflictFixBusy.value) return;
  const ok = await confirm(
    `即将从系统 PATH 清除 ${list.length} 条 MySQL 目录：\n\n${list.join("\n")}\n\n需要管理员权限（会弹 UAC 确认）。`,
    { title: "一键修复 MySQL PATH 冲突", kind: "warning" }
  );
  if (!ok) return;
  mysqlConflictFixBusy.value = true;
  try {
    const result: DevToolsInfo = await invoke("fix_mysql_path_conflicts", { paths: list });
    devTools.value = result;
    const remaining = result.mysql?.conflicts.length ?? 0;
    if (remaining === 0) {
      toast.success("已清理。请**新开** cmd 跑 `where mysql` 验证。");
    } else {
      toast.warn(`部分路径仍在系统 PATH（剩 ${remaining} 条）`);
    }
  } catch (e) { showError(`修复失败: ${e}`); }
  finally { mysqlConflictFixBusy.value = false; }
}

function mysqlSourceLabel(s: string): string {
  switch (s) {
    case "store": return "NX";
    case "phpstudy": return "PS";
    case "manual": return "独立";
    case "system": return "系统";
    default: return s;
  }
}

onMounted(async () => {
  // 三个请求并发拉，全部回来再放下 loading 屏，避免逐个填充导致的"未发现 X"误导
  await Promise.all([loadServices(), loadGlobalPhp(), loadDevTools()]);
  loaded.value = true;
  // composer 信息回来之后才能读镜像源，串到 loadDevTools 之后
  loadComposerRepo();
  unlistenServicesChanged = await listen("services-changed", () => {
    loadServices();
    loadGlobalPhp();
    loadDevTools().then(loadComposerRepo);
  });
});

onUnmounted(() => {
  if (unlistenServicesChanged) unlistenServicesChanged();
});
</script>

<template>
  <div>
    <p class="page-subtitle">配置命令行 <code>php</code> / <code>composer</code> / <code>node</code> / <code>mysql</code> 走哪个版本，以及 MySQL root 密码。</p>

    <!-- 首屏加载占位：4 张卡的骨架，避免空状态闪烁 -->
    <template v-if="!loaded">
      <div v-for="i in 4" :key="i" class="card mb-3 env-skeleton">
        <div class="skel-bar skel-title"></div>
        <div class="skel-bar skel-line"></div>
        <div class="skel-bar skel-line w-2/3"></div>
      </div>
    </template>

    <template v-else>
    <!-- PHP -->
    <div class="card mb-3">
      <h2 class="env-section-title">PHP（命令行 <code>php</code>）</h2>
      <div v-if="phpServices.length === 0" class="env-empty">未发现 PHP 安装。可在「软件商店」安装。</div>
      <template v-else>
        <div class="env-row">
          <span class="env-label">当前全局</span>
          <span v-if="globalPhp.version" class="env-value">v{{ globalPhp.version }}</span>
          <span v-else class="env-value env-warn">未设置</span>
        </div>
        <div class="env-row">
          <span class="env-label">切换到</span>
          <SelectMenu
            v-model="globalPhpPick"
            :options="phpServices.map(p => ({ label: phpOptionLabel(p), value: p.version }))"
            trigger-class="version-sel version-sel-btn"
          />
          <button class="btn btn-primary btn-sm ml-auto"
                  :disabled="!globalPhpPick || globalPhpBusy || globalPhpPick === globalPhp.version"
                  @click="applyGlobalPhp">
            {{ globalPhpBusy ? '切换中...' : (globalPhpPick === globalPhp.version ? '已是全局' : '设为全局') }}
          </button>
        </div>

        <div v-if="globalPhp.conflicts.length > 0" class="conflict-banner mt-3">
          <div class="flex items-start gap-2">
            <AlertTriangle :size="16" class="shrink-0 mt-0.5" style="color: var(--color-warn, #f59e0b)" />
            <div class="flex-1 text-[13px]">
              <div class="font-semibold mb-1">全局切换可能不生效</div>
              <div class="mb-1" style="color: var(--text-secondary)">
                系统 PATH 里有 {{ globalPhp.conflicts.length }} 条 PHP 目录排在本应用前面，<code>php -v</code> 会优先命中它们。
              </div>
              <div class="font-mono text-[13px] mb-2 pl-2" style="color: var(--text-muted)">
                <div v-for="c in globalPhp.conflicts" :key="c">· {{ c }}</div>
              </div>
              <div class="flex items-center gap-2 flex-wrap">
                <button class="btn btn-primary btn-sm"
                        :disabled="conflictFixBusy"
                        @click="fixConflicts">
                  {{ conflictFixBusy ? '修复中...' : '一键修复（需管理员）' }}
                </button>
                <button class="btn btn-secondary btn-sm" @click="openEnvEditor">打开环境变量窗口</button>
                <button class="text-[13px] underline cursor-pointer"
                        style="color: var(--color-blue-light); background: transparent; border: none"
                        @click="showConflictHelp = !showConflictHelp">
                  {{ showConflictHelp ? '隐藏手动步骤' : '手动步骤？' }}
                </button>
              </div>
              <div v-if="showConflictHelp" class="mt-2 text-[13px] leading-relaxed"
                   style="color: var(--text-secondary)">
                <ol class="list-decimal pl-4 space-y-0.5">
                  <li>点上面"打开环境变量窗口"按钮</li>
                  <li>在<b>系统变量</b>区双击 <code>Path</code></li>
                  <li>删掉上述列出的那几条，一路确定</li>
                  <li><b>新开</b>命令行窗口，运行 <code>where php</code> 验证</li>
                </ol>
              </div>
            </div>
          </div>
        </div>
      </template>
    </div>

    <!-- Composer -->
    <div class="card mb-3">
      <h2 class="env-section-title">Composer</h2>
      <div v-if="!devTools.composer" class="env-empty">未发现 Composer。可在「软件商店」安装。</div>
      <template v-else>
        <div class="env-row">
          <span class="env-label">当前全局</span>
          <span v-if="devTools.composer.active_version" class="env-value">v{{ devTools.composer.active_version }}</span>
          <span v-else class="env-value env-warn">未激活</span>
        </div>
        <div class="env-row">
          <template v-if="devTools.composer.available.length > 1">
            <span class="env-label">切换到</span>
            <SelectMenu
              v-model="composerPick"
              :options="devTools.composer.available.map(a => ({ label: `v${a.version} [${sourceLabel(a.source)}]`, value: a.version }))"
              trigger-class="version-sel version-sel-btn"
            />
            <button class="btn btn-primary btn-sm ml-auto"
                    :disabled="!composerPick || composerSwitchBusy || composerPick === devTools.composer.active_version"
                    @click="switchComposer">
              {{ composerSwitchBusy ? '切换中...' : (composerPick === devTools.composer.active_version ? '当前' : '设为全局') }}
            </button>
          </template>
          <template v-else>
            <span class="env-label">来源</span>
            <span class="env-value">
              {{ devTools.composer.available[0]?.source === 'system' ? '系统' : '商店' }}
            </span>
          </template>
        </div>

        <!-- 镜像源切换 -->
        <div v-if="devTools.composer.active_version" class="env-row">
          <span class="env-label">镜像源</span>
          <SelectMenu
            v-model="composerMirrorPick"
            :options="composerMirrors"
            trigger-class="version-sel version-sel-btn"
          />
          <input
            v-if="composerMirrorPick === 'custom'"
            v-model="composerCustomUrl"
            class="input ml-2 flex-1 min-w-0"
            placeholder="https://your-mirror.example.com/composer/"
          />
          <button class="btn btn-primary btn-sm ml-auto"
                  :disabled="composerRepoBusy
                             || (composerMirrorPick !== 'custom' && composerMirrorPick === composerRepoUrl)
                             || (composerMirrorPick === 'custom' && composerCustomUrl.trim() === composerRepoUrl)"
                  @click="applyComposerMirror">
            {{ composerRepoBusy ? '应用中...' : '应用' }}
          </button>
        </div>
        <div v-if="devTools.composer.active_version" class="env-row" style="padding-top: 0">
          <span class="env-label"></span>
          <span class="env-value" style="font-size: 12px; color: var(--text-muted)">
            当前：{{ composerRepoUrl || 'Packagist 官方' }}
          </span>
        </div>
      </template>
    </div>

    <!-- Node.js -->
    <div class="card mb-3">
      <h2 class="env-section-title">
        Node.js
        <span v-if="devTools.nvm" class="env-section-sub">via NVM v{{ devTools.nvm.nvm_version }} · {{ devTools.nvm.nvm_source === 'system' ? '系统' : '商店' }}</span>
      </h2>
      <div v-if="!devTools.nvm" class="env-empty">未发现 NVM。可在「软件商店」安装 nvm-windows。</div>
      <template v-else>
        <div class="env-row">
          <span class="env-label">当前 Node</span>
          <span v-if="devTools.nvm.current_node" class="env-value">v{{ devTools.nvm.current_node }}</span>
          <span v-else class="env-value env-warn">未启用</span>
          <span class="env-path" :title="devTools.nvm.nvm_home">{{ devTools.nvm.nvm_home }}</span>
        </div>
        <div class="env-row">
          <template v-if="devTools.nvm.installed_nodes.length > 0">
            <span class="env-label">已安装</span>
            <SelectMenu
              v-model="nodePick"
              :options="devTools.nvm.installed_nodes.map(v => ({
                label: `v${v}${v === devTools.nvm!.current_node ? ' ●' : ''}`,
                value: v
              }))"
              trigger-class="version-sel version-sel-btn"
            />
            <button class="btn btn-primary btn-sm ml-auto"
                    :disabled="!nodePick || nodeSwitchBusy || nodeInstallBusy || nodeUninstallBusy || nodePick === devTools.nvm.current_node"
                    @click="switchNode">
              {{ nodeSwitchBusy ? '切换中...' : (nodePick === devTools.nvm.current_node ? '当前使用' : '使用 / 切换') }}
            </button>
            <button class="btn btn-danger btn-sm"
                    :disabled="!nodePick || nodeSwitchBusy || nodeInstallBusy || nodeUninstallBusy || nodePick === devTools.nvm.current_node"
                    :title="nodePick === devTools.nvm.current_node ? '请先切换到其他版本' : '卸载所选版本'"
                    @click="uninstallNode">
              {{ nodeUninstallBusy ? '卸载中...' : '卸载' }}
            </button>
          </template>
          <template v-else>
            <span class="env-label">已安装</span>
            <span class="env-empty">暂无 Node.js 版本，请在下方下载安装。</span>
          </template>
        </div>
        <div class="env-row">
          <span class="env-label">可下载</span>
          <template v-if="nodeCatalogLoaded">
            <SelectMenu
              v-if="nodeDownloadOptions.length > 0"
              v-model="nodeInstallPick"
              :options="nodeDownloadOptions"
              trigger-class="version-sel version-sel-btn node-version-sel"
            />
            <span v-else class="env-empty">列表中的版本均已安装</span>
          </template>
          <span v-else class="env-empty">点击获取 NVM 官方版本列表</span>
          <button class="btn btn-secondary btn-sm ml-auto"
                  :disabled="nodeCatalogBusy || nodeInstallBusy"
                  @click="loadNodeCatalog">
            {{ nodeCatalogBusy ? '获取中...' : (nodeCatalogLoaded ? '刷新列表' : '获取版本') }}
          </button>
        </div>
        <div class="env-row">
          <span class="env-label">精确版本</span>
          <input
            v-model="nodeCustomVersion"
            class="input node-version-input"
            placeholder="可选，如 22.22.3；填写后优先"
            :disabled="nodeInstallBusy"
            @keyup.enter="installNode"
          />
          <label class="node-activate-check">
            <input v-model="nodeActivateAfterInstall" type="checkbox" :disabled="nodeInstallBusy" />
            安装后立即使用
          </label>
          <button class="btn btn-primary btn-sm ml-auto"
                  :disabled="nodeInstallBusy || nodeSwitchBusy || nodeUninstallBusy || (!nodeCustomVersion.trim() && !nodeInstallPick)"
                  @click="installNode">
            {{ nodeInstallBusy ? '下载并安装中...' : '下载并安装' }}
          </button>
        </div>
        <div class="node-tip">
          安装由 nvm-windows 完成；“安装后立即使用”会在下载完成后自动执行 <code>nvm use</code>。
        </div>
      </template>
    </div>

    <!-- MySQL -->
    <div class="card mb-3">
      <h2 class="env-section-title">MySQL</h2>
      <div v-if="!devTools.mysql || devTools.mysql.available.length === 0" class="env-empty">未发现 MySQL 安装。可在「软件商店」安装。</div>
      <template v-else>
        <div class="env-row">
          <span class="env-label">当前全局</span>
          <span v-if="devTools.mysql.active_version" class="env-value">v{{ devTools.mysql.active_version }}</span>
          <span v-else class="env-value env-warn">未设全局</span>
          <span v-if="currentMysqlOption" class="env-port">:{{ currentMysqlOption.port }}</span>
          <span v-if="currentMysqlOption" class="env-source-badge">{{ mysqlSourceLabel(currentMysqlOption.source) }}</span>
        </div>
        <div class="env-row">
          <span class="env-label">切换到</span>
          <SelectMenu
            v-model="mysqlPick"
            :options="devTools.mysql.available.map(a => ({
              label: `v${a.version} [${mysqlSourceLabel(a.source)}]${a.version === devTools.mysql!.active_version ? ' ●' : ''}`,
              value: a.version
            }))"
            trigger-class="version-sel version-sel-btn"
          />
          <button class="btn btn-primary btn-sm ml-auto"
                  :disabled="!mysqlPick || mysqlSwitchBusy || mysqlPick === devTools.mysql.active_version"
                  @click="switchMysql">
            {{ mysqlSwitchBusy ? '切换中...' : (mysqlPick === devTools.mysql.active_version ? '当前' : '设为全局') }}
          </button>
        </div>

        <!-- 系统 PATH 冲突横条：HKLM 里有别的 mysqld 目录会屏蔽用户 PATH 切换 -->
        <div v-if="devTools.mysql.conflicts.length > 0" class="conflict-banner mt-3">
          <div class="flex items-start gap-2">
            <AlertTriangle :size="16" class="shrink-0 mt-0.5" style="color: var(--color-warn)" />
            <div class="flex-1 text-[13px]">
              <div class="font-semibold mb-1">"设为全局"可能不生效</div>
              <div class="mb-1" style="color: var(--text-secondary)">
                系统 PATH 里有 {{ devTools.mysql.conflicts.length }} 条 MySQL 目录排在用户 PATH 前面，<code>mysql --version</code> 会优先命中它们。
              </div>
              <div class="font-mono text-[13px] mb-2 pl-2" style="color: var(--text-muted)">
                <div v-for="c in devTools.mysql.conflicts" :key="c">· {{ c }}</div>
              </div>
              <button class="btn btn-primary btn-sm"
                      :disabled="mysqlConflictFixBusy"
                      @click="fixMysqlPathConflicts">
                {{ mysqlConflictFixBusy ? '修复中...' : '一键修复（需管理员）' }}
              </button>
            </div>
          </div>
        </div>

        <div v-if="currentMysqlOption" class="env-row">
          <span class="env-label">root 密码</span>
          <div class="env-pwd-wrap">
            <input
              :type="showMysqlPwd ? 'text' : 'password'"
              v-model="mysqlPwdInput"
              :placeholder="currentMysqlOption.root_password ? '已保存当前密码（输入新密码可修改）' : '输入 root 密码'"
              class="env-pwd-input"
            />
            <button class="env-pwd-toggle" type="button"
                    :title="showMysqlPwd ? '隐藏' : '显示'"
                    @click="showMysqlPwd = !showMysqlPwd">
              <component :is="showMysqlPwd ? EyeOff : Eye" :size="16" />
            </button>
          </div>
          <button class="btn btn-primary btn-sm"
                  :disabled="!mysqlPwdInput || mysqlPwdBusy"
                  @click="saveMysqlPassword">
            {{ mysqlPwdBusy ? '修改中...' : '修改' }}
          </button>
        </div>
        <!-- 当前密码字段：仅在没存过 .naxone-root.txt 时显示（PHPStudy 等外来 MySQL） -->
        <div v-if="currentMysqlOption && !currentMysqlOption.root_password" class="env-row">
          <span class="env-label">当前密码</span>
          <input
            type="password"
            v-model="mysqlCurrentPwd"
            placeholder="留空则尝试空密码（PHPStudy 默认是 root）"
            class="env-pwd-input"
          />
        </div>
      </template>
    </div>
    </template>
  </div>
</template>

<style scoped>
.env-skeleton {
  pointer-events: none;
}
.skel-bar {
  background: var(--bg-tertiary);
  border-radius: 4px;
  height: 10px;
  margin-bottom: 6px;
  animation: skel-pulse 1.2s ease-in-out infinite;
}
.skel-title {
  height: 14px;
  width: 40%;
  margin-bottom: 12px;
}
.skel-line {
  width: 100%;
}
@keyframes skel-pulse {
  0%, 100% { opacity: 0.6; }
  50% { opacity: 0.95; }
}

.page-subtitle {
  font-size: 13px;
  color: var(--text-muted);
  margin-bottom: 12px;
}
.env-section-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text-primary);
  margin-bottom: 10px;
  display: flex;
  align-items: baseline;
  gap: 8px;
}
.env-section-sub {
  font-size: 13px;
  font-weight: 400;
  color: var(--text-muted);
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
}
.env-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 0;
}
.env-row + .env-row {
  border-top: 1px solid var(--border-color);
}
.env-label {
  font-size: 13px;
  color: var(--text-muted);
  min-width: 72px;
  flex-shrink: 0;
}
.env-value {
  font-size: 13px;
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  color: var(--color-success-light);
}
.env-path {
  min-width: 0;
  margin-left: auto;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  color: var(--text-muted);
}
.env-port {
  font-size: 13px;
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  color: var(--text-muted);
}
.env-source-badge {
  font-size: 13px;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 4px;
  background: var(--bg-tertiary);
  color: var(--text-secondary);
}
.env-warn {
  color: var(--color-warn, #f59e0b);
}
.env-empty {
  font-size: 13px;
  color: var(--text-muted);
}
.env-pwd-wrap {
  position: relative;
  flex: 1;
  min-width: 0;
  display: flex;
}
.env-pwd-input {
  width: 100%;
  height: 28px;
  padding: 0 36px 0 10px;
  font-size: 13px;
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  background: var(--bg-primary);
  border: 1px solid var(--border-color);
  border-radius: 6px;
  color: var(--text-primary);
}
.env-pwd-input:focus {
  outline: none;
  border-color: var(--border-focus);
}
.env-pwd-toggle {
  position: absolute;
  right: 4px;
  top: 50%;
  transform: translateY(-50%);
  width: 24px;
  height: 24px;
  border: none;
  background: transparent;
  border-radius: 4px;
  color: var(--text-muted);
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  transition: color 150ms ease, background 150ms ease;
}
.env-pwd-toggle:hover {
  color: var(--text-primary);
  background: var(--bg-hover);
}
.node-version-input {
  width: 190px;
  min-width: 140px;
  height: 30px;
  box-sizing: border-box;
  padding: 0 8px;
  font-size: 13px;
  line-height: 1.25;
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
}
.node-activate-check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  white-space: nowrap;
  font-size: 13px;
  color: var(--text-secondary);
  cursor: pointer;
}
.node-activate-check input {
  accent-color: var(--color-primary);
}
.node-tip {
  padding: 4px 0 0 80px;
  font-size: 12px;
  color: var(--text-muted);
}

.conflict-banner {
  background: rgba(245, 158, 11, 0.08);
  border: 1px solid rgba(245, 158, 11, 0.2);
  border-radius: 8px;
  padding: 10px 12px;
}

:deep(.rs-select.version-sel) {
  display: inline-flex;
  width: auto;
  max-width: 100%;
}
:deep(.rs-select-trigger.version-sel-btn) {
  width: auto;
  min-width: 140px;
}
:deep(.rs-select-trigger.node-version-sel) {
  min-width: 190px;
}

code {
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  background: var(--bg-tertiary);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 13px;
}
</style>
