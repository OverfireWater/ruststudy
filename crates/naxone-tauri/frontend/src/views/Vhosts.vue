<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useRouter } from "vue-router";
import { toast, friendlyError } from "../composables/useToast";
import { onTextareaTab } from "../composables/useTextareaTab";
import SelectMenu from "../components/SelectMenu.vue";
import ConfirmDialog from "../components/ConfirmDialog.vue";
import { Pencil, ExternalLink, FolderOpen, Trash2, FileText, AlertCircle } from "lucide-vue-next";

const router = useRouter();

interface VhostInfo {
  id: string; server_name: string; aliases: string[]; listen_port: number;
  document_root: string; php_version: string | null; index_files: string;
  rewrite_rule: string; autoindex: boolean; has_ssl: boolean;
  ssl_cert: string; ssl_key: string; force_https: boolean;
  custom_directives: string; access_log: string; enabled: boolean;
  created_at: string; expires_at: string;
  source: string; source_name?: string | null;
}

interface PhpVersionInfo { label: string; version: string; port: number; install_path: string; }

interface FormData {
  server_name: string; aliases: string; listen_port: number; document_root: string;
  php_version: string | null; index_files: string; rewrite_rule: string;
  autoindex: boolean; ssl_cert: string | null; ssl_key: string | null;
  force_https: boolean; custom_directives: string | null; access_log: string | null;
  sync_hosts: boolean; expires_at: string;
}

const rewritePresets = [
  { label: "通用（默认）", value: "" },
  { label: "ThinkPHP", value: "location / {\n    if (!-e $request_filename) {\n        rewrite ^(.*)$ /index.php?s=$1 last;\n        break;\n    }\n}" },
  { label: "Laravel", value: "location / {\n    try_files $uri $uri/ /index.php?$query_string;\n}" },
  { label: "WordPress", value: "location / {\n    try_files $uri $uri/ /index.php?$args;\n}" },
  { label: "Webman", value: "location / {\n    proxy_pass http://127.0.0.1:8787;\n    proxy_http_version 1.1;\n    proxy_set_header Host $host;\n    proxy_set_header X-Real-IP $remote_addr;\n    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n}" },
  { label: "自定义", value: "__custom__" },
];

const templateOptions = [
  { label: "无（不动目录）", value: "none" },
  { label: "空白（phpinfo）", value: "blank" },
  { label: "WordPress（中文）", value: "wordpress" },
  { label: "Laravel", value: "laravel" },
  { label: "ThinkPHP", value: "thinkphp" },
  { label: "Webman（cli 常驻）", value: "webman" },
];
const boolOptions = [
  { label: "关闭", value: false },
  { label: "开启", value: true },
];
const phpVersionOptions = computed(() => [
  { label: "不使用 PHP", value: null },
  ...phpVersions.value.map((pv) => ({ label: `${pv.label} (:${pv.port})`, value: pv.version })),
]);
const rewritePresetOptions = rewritePresets.map((preset) => ({ label: preset.label, value: preset.value }));

const vhosts = ref<VhostInfo[]>([]);
const phpVersions = ref<PhpVersionInfo[]>([]);
const floatMsg = ref("");
const floatType = ref<"success" | "error">("success");
const busy = ref(false);
const searchQuery = ref("");
const showForm = ref(false);
const editingId = ref<string | null>(null);
const editingSource = ref<string>("");
const showTakeover = ref(false);
const pendingDelete = ref<VhostInfo | null>(null);

// null = 还在检测；false = 没有任何 nginx/apache；true = 至少装了一个
const hasWebServer = ref<boolean | null>(null);

// 站点模板初始化（仅新建时显示）
const initTemplate = ref<string>("none");
const showTemplateModal = ref(false);
const templateLogs = ref<string[]>([]);
const templateBusy = ref(false);
interface TemplateProgress {
  phase: "downloading" | "extracting" | "running" | "done";
  current?: number;
  total?: number;
  unit?: string;
}
const templateProgress = ref<TemplateProgress | null>(null);
const templateElapsed = ref<number>(0);
// 失败 / 重试 / 换镜像 state
const templateFailed = ref(false);
const templateLastTarget = ref("");
const templateLastTpl = ref("");
// 镜像列表：跟「全局环境」composer 镜像下拉保持一致。value 用 URL，空串 = 官方。
const templateMirrorOptions = [
  { label: "Packagist 官方", value: "" },
  { label: "腾讯云", value: "https://mirrors.cloud.tencent.com/composer/" },
  { label: "华为云", value: "https://mirrors.huaweicloud.com/repository/php/" },
  { label: "阿里云", value: "https://mirrors.aliyun.com/composer/" },
];
const templateMirrorPick = ref<string>(""); // 当前下拉选中的 URL（响应式）
const templateCancelBusy = ref(false);
let templateTimer: ReturnType<typeof setInterval> | null = null;

function phaseLabel(p: TemplateProgress | null, busy: boolean): string {
  if (!busy && p?.phase === "done") return "完成";
  if (!p) return "准备中…";
  return { downloading: "下载中", extracting: "解压中", running: "执行中", done: "完成" }[p.phase];
}
function progressPercent(p: TemplateProgress | null): number {
  if (!p?.current || !p?.total) return 0;
  return Math.min(100, Math.round((p.current / p.total) * 100));
}
function progressText(p: TemplateProgress | null): string {
  if (!p || p.current === undefined) return "";
  if (p.unit === "MB") {
    const cur = (p.current / 1024 / 1024).toFixed(1);
    const tot = p.total ? (p.total / 1024 / 1024).toFixed(1) : "?";
    return `${cur} / ${tot} MB`;
  }
  if (p.unit === "文件") return `${p.current} / ${p.total ?? "?"} 文件`;
  return "";
}

// Raw conf 编辑
const showConfModal = ref(false);
const confEditingId = ref<string>("");
const confEditingDomain = ref<string>("");
const confPath = ref<string>("");
const confContent = ref<string>("");
const confOriginal = ref<string>("");
const confSaving = ref(false);

async function openConfEdit(vh: VhostInfo) {
  confEditingId.value = vh.id;
  confEditingDomain.value = `${vh.server_name}:${vh.listen_port}`;
  confContent.value = "";
  confOriginal.value = "";
  confPath.value = "";
  showConfModal.value = true;
  try {
    const res = await invoke<{ path: string; content: string }>("read_vhost_conf", { id: vh.id });
    confPath.value = res.path;
    confContent.value = res.content;
    confOriginal.value = res.content;
  } catch (e) { showError("读取配置失败: " + e); showConfModal.value = false; }
}

async function saveConfEdit() {
  if (confSaving.value) return;
  if (confContent.value === confOriginal.value) { showConfModal.value = false; return; }
  confSaving.value = true;
  try {
    await invoke("write_vhost_conf", { id: confEditingId.value, content: confContent.value });
    showSuccess("配置已保存，nginx 已 reload");
    showConfModal.value = false;
  } catch (e) { showError("保存失败: " + e); }
  finally { confSaving.value = false; }
}
const modalTab = ref<"basic" | "rewrite" | "ssl">("basic");
const rewritePreset = ref("");
// WWW 根目录（用户在设置里配的），新建 vhost 时用它作 document_root 默认值
const wwwRoot = ref<string>("");
/// 是否用户手动改过 document_root（没改过时，随 server_name 输入自动拼 {www_root}/{server_name}）
const docRootTouched = ref(false);

const form = ref<FormData>({
  server_name: "", aliases: "", listen_port: 80, document_root: "",
  php_version: null, index_files: "index.php index.html", rewrite_rule: "",
  autoindex: false, ssl_cert: null, ssl_key: null, force_https: false,
  custom_directives: null, access_log: null,
  sync_hosts: true, expires_at: "",
});

function formatDate(d: string): string {
  if (!d) return "";
  // Handle both yyyy-MM-dd and yyyy/MM/dd
  return d.replace(/-/g, "/");
}

function showError(msg: unknown) { toast.error(friendlyError(msg)); }
function showSuccess(msg: string) { toast.success(String(msg)); }
let floatTimer: number | null = null;
function showFloat(msg: string, type: "success" | "error" = "success") {
  if (floatTimer) clearTimeout(floatTimer);
  floatMsg.value = msg; floatType.value = type;
  floatTimer = window.setTimeout(() => { floatMsg.value = ""; floatTimer = null; }, 2500);
}
async function loadVhosts() { try { vhosts.value = await invoke("get_vhosts"); } catch (e) { showError("加载失败: " + e); } }

interface ServiceInfo { kind: string; }
async function checkWebServer() {
  try {
    const services: ServiceInfo[] = await invoke("get_services");
    hasWebServer.value = services.some(s => s.kind === "nginx" || s.kind === "apache");
  } catch {
    // 失败时保守按"有"处理，避免误导用户去装包
    hasWebServer.value = true;
  }
}
async function loadPhpVersions() { try { phpVersions.value = await invoke("get_php_versions"); } catch (e) { showError("加载PHP版本失败: " + e); } }
async function loadWwwRoot() {
  try {
    const cfg = await invoke<{ www_root: string }>("get_config");
    wwwRoot.value = (cfg.www_root || "").replace(/\\/g, "/").replace(/\/+$/, "");
  } catch { /* 非致命：用空字符串兜底让用户自己填 */ }
}

/// 把 server_name 拼成 {www_root}/{server_name} 作为推荐 document_root
function suggestedDocRoot(serverName: string): string {
  const name = (serverName || "").trim();
  if (!wwwRoot.value) return "";
  if (!name) return wwwRoot.value + "/";
  return `${wwwRoot.value}/${name}`;
}

function openCreate() {
  editingId.value = null; modalTab.value = "basic"; rewritePreset.value = "";
  docRootTouched.value = false;
  initTemplate.value = "none";
  // document_root 默认用 www_root，没配就留空让用户自己填
  const defaultRoot = wwwRoot.value ? `${wwwRoot.value}/` : "";
  form.value = { server_name: "", aliases: "", listen_port: 80, document_root: defaultRoot,
    php_version: phpVersions.value.length > 0 ? phpVersions.value[0].version : null,
    index_files: "index.php index.html", rewrite_rule: "", autoindex: false,
    ssl_cert: null, ssl_key: null, force_https: false, custom_directives: null, access_log: null,
    sync_hosts: true, expires_at: "" };
  showForm.value = true;
}

function openEdit(vh: VhostInfo) {
  editingId.value = vh.id;
  editingSource.value = vh.source;
  modalTab.value = "basic";
  docRootTouched.value = true;  // 编辑时不自动改
  const matched = rewritePresets.find(p => p.value === vh.rewrite_rule && p.value !== "__custom__");
  rewritePreset.value = matched ? matched.value : (vh.rewrite_rule ? "__custom__" : "");
  form.value = { server_name: vh.server_name, aliases: vh.aliases.join(" "), listen_port: vh.listen_port,
    document_root: vh.document_root, php_version: vh.php_version, index_files: vh.index_files,
    rewrite_rule: vh.rewrite_rule, autoindex: vh.autoindex, ssl_cert: vh.ssl_cert || null,
    ssl_key: vh.ssl_key || null, force_https: vh.force_https,
    custom_directives: vh.custom_directives || null, access_log: vh.access_log || null,
    sync_hosts: true, expires_at: formatDate(vh.expires_at || "") };
  showForm.value = true;
}

/// server_name 输入时如果 document_root 还没被手动改过，自动拼成 {www_root}/{server_name}
function onServerNameInput() {
  if (!editingId.value && !docRootTouched.value) {
    form.value.document_root = suggestedDocRoot(form.value.server_name);
  }
}

function onRewritePresetChange(val: string) { rewritePreset.value = val; if (val !== "__custom__") form.value.rewrite_rule = val; }
function cancelForm() { showForm.value = false; editingId.value = null; editingSource.value = ""; }

async function toggleVhost(vh: VhostInfo) {
  busy.value = true;
  try {
    vhosts.value = await invoke("toggle_vhost", { id: vh.id, enabled: !vh.enabled });
    showFloat(vh.enabled ? "站点已停用" : "站点已启用");
  } catch (e) { showFloat("操作失败: " + e, "error"); } finally { busy.value = false; }
}

async function openSite(vh: VhostInfo) {
  const protocol = vh.has_ssl ? "https" : "http";
  const port = vh.listen_port === 80 || (vh.has_ssl && vh.listen_port === 443) ? "" : `:${vh.listen_port}`;
  await invoke("open_in_browser", { url: `${protocol}://${vh.server_name}${port}` });
}

async function openDir(vh: VhostInfo) {
  await invoke("open_folder", { path: vh.document_root });
}

async function saveVhost() {
  if (!form.value.server_name.trim()) { showError("域名不能为空"); return; }
  if (!form.value.document_root.trim()) { showError("网站目录不能为空"); return; }
  // Check duplicate domain:port
  const newId = `${form.value.server_name}_${form.value.listen_port}`;
  const dup = vhosts.value.find(v => v.id === newId && v.id !== editingId.value);
  if (dup) { showError(`站点 ${form.value.server_name}:${form.value.listen_port} 已存在`); return; }

  // 编辑 PHPStudy 站点时，保存 = 接管。先弹确认对话框。
  if (editingId.value && editingSource.value === "phpstudy") {
    showTakeover.value = true;
    return;
  }

  await doSaveVhost();
}

/// 框架真实入口在项目根下的 public/，模板装完后自动指向那里，否则浏览器看到一堆裸文件找不到入口
/// Webman 也用 public（nginx 走 proxy_pass 不依赖目录，但保留 public 让静态资源走 nginx 直出）
function entrySubdirForTemplate(t: string): string {
  return ["laravel", "thinkphp", "webman"].includes(t) ? "public" : "";
}

/// 选模板时自动配上对应伪静态（用户去"伪静态" tab 能看到，可手改）
const templateToRewriteLabel: Record<string, string> = {
  laravel: "Laravel",
  thinkphp: "ThinkPHP",
  wordpress: "WordPress",
  webman: "Webman",
};
watch(initTemplate, (val) => {
  if (editingId.value) return; // 编辑模式不自动改
  const targetLabel = templateToRewriteLabel[val];
  if (!targetLabel) return; // none / blank 不动当前规则
  const preset = rewritePresets.find((p) => p.label === targetLabel);
  if (preset) {
    rewritePreset.value = preset.value;
    form.value.rewrite_rule = preset.value;
  }
  // Webman 走 proxy_pass，不需要 nginx 拉 fastcgi → 把 PHP 版本清空，避免生成无效 fastcgi 块
  if (val === "webman") {
    form.value.php_version = null;
  }
});

async function doSaveVhost() {
  busy.value = true;
  try {
    const isEdit = !!editingId.value;
    const templateToInit = !isEdit && initTemplate.value !== "none" ? initTemplate.value : null;
    const projectRoot = form.value.document_root;
    const formSnap = { ...form.value };
    const vhostId = `${formSnap.server_name}_${formSnap.listen_port}`;
    if (isEdit) vhosts.value = await invoke("update_vhost", { id: editingId.value, req: form.value });
    else vhosts.value = await invoke("create_vhost", { req: form.value });
    showForm.value = false; editingId.value = null; editingSource.value = "";
    showSuccess(isEdit ? "站点更新成功，已自动 reload Web 服务器" : "站点创建成功，已自动 reload Web 服务器");
    if (templateToInit) {
      const ok = await runTemplateInit(projectRoot, templateToInit);
      if (!ok) return; // 模板失败：不再追加误导性的 webman/入口子目录提示
      const subdir = entrySubdirForTemplate(templateToInit);
      if (subdir) {
        const newRoot = `${projectRoot}/${subdir}`.replace(/\\/g, "/").replace(/\/+/g, "/");
        try {
          vhosts.value = await invoke("update_vhost", {
            id: vhostId,
            req: { ...formSnap, document_root: newRoot },
          });
          showFloat(`已自动指向入口目录 ${subdir}/`);
        } catch (e) {
          showError(`自动设置入口目录失败，请手动改 nginx root: ${e}`);
        }
      }
      // Webman 是常驻 cli 进程，nginx 仅做 proxy_pass，必须用户手动起
      if (templateToInit === "webman") {
        templateLogs.value.push("");
        templateLogs.value.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        templateLogs.value.push("⚠ Webman 是常驻 cli 进程，需手动启动：");
        templateLogs.value.push(`  cd ${projectRoot}`);
        templateLogs.value.push("  双击 windows.bat 或运行 php windows.php");
        templateLogs.value.push("启动后浏览器访问本站点即可（nginx 已配 proxy 到 :8787）");
      }
    }
  } catch (e) { showError("保存失败: " + e); } finally { busy.value = false; }
}

function copyTemplateLogs() {
  navigator.clipboard.writeText(templateLogs.value.join("\n"));
  showFloat("日志已复制");
}

/// 停止当前装包：调后端 taskkill 杀子进程树
async function cancelTemplate() {
  if (templateCancelBusy.value) return;
  templateCancelBusy.value = true;
  try {
    const killed = await invoke<boolean>("cancel_init_site_template");
    if (killed) {
      templateLogs.value.push("⏹ 用户主动停止");
      toast.info("已停止装包");
    } else {
      toast.info("没有正在运行的装包任务");
    }
  } catch (e) {
    showError(`停止失败: ${e}`);
  } finally {
    templateCancelBusy.value = false;
  }
}

/// 重试：清空日志重新跑相同的 target + template
async function retryTemplate() {
  if (templateBusy.value || !templateLastTarget.value || !templateLastTpl.value) return;
  // 先清掉上次失败留下的残留（composer 写了一半的 vendor/composer.json 等），
  // 否则 init_site_template 的目录非空校验会再次失败。
  // 注：cleanup 日志通过 prefixLog 传给 runTemplateInit —— 因为后者入口会清空 templateLogs，
  // 这里 push 也会被擦掉。
  let prefix: string | undefined;
  try {
    const cleaned = await invoke<number>("cleanup_template_dir", {
      targetDir: templateLastTarget.value,
    });
    if (cleaned > 0) prefix = `🧹 已清理 ${cleaned} 项失败残留`;
  } catch (e) {
    showError(`清理失败残留失败: ${e}`);
    return;
  }
  await runTemplateInit(templateLastTarget.value, templateLastTpl.value, prefix);
}

/// 用户从下拉选了镜像 → 切换 + 自动重试
async function onMirrorPicked(value: string | number | boolean | null) {
  if (templateBusy.value) return;
  const url = typeof value === "string" ? value : "";
  const opt = templateMirrorOptions.find((m) => m.value === url);
  const label = opt?.label || url || "(未知)";
  try {
    await invoke("set_composer_repo", { url });
    templateLogs.value.push(`🔄 镜像源已切到「${label}」${url ? ` (${url})` : ""}，重试中...`);
    toast.success(`镜像源 → ${label}`);
    await retryTemplate();
  } catch (e) {
    showError(`切换镜像源失败: ${e}`);
  }
}

/// 失败时把当前镜像同步到下拉，方便用户看到当前值再选别的
async function syncCurrentMirror() {
  try {
    templateMirrorPick.value = await invoke<string>("get_composer_repo");
  } catch {
    templateMirrorPick.value = "";
  }
}

async function runTemplateInit(
  targetDir: string,
  template: string,
  prefixLog?: string, // 在清空日志后立即 push 这一条（重试时用来回显 cleanup 结果）
): Promise<boolean> {
  showTemplateModal.value = true;
  templateLogs.value = [];
  if (prefixLog) templateLogs.value.push(prefixLog);
  templateBusy.value = true;
  templateFailed.value = false;
  templateLastTarget.value = targetDir;
  templateLastTpl.value = template;
  templateProgress.value = null;
  templateElapsed.value = 0;
  const start = Date.now();
  templateTimer = setInterval(() => {
    templateElapsed.value = Math.round((Date.now() - start) / 1000);
  }, 1000);

  const { listen } = await import("@tauri-apps/api/event");
  // composer 在 stderr 输出 ANSI 颜色码（如 \x1b[32m）让 <pre> 显示乱码，剥掉。
  const ansiRe = /\x1b\[[0-9;]*[a-zA-Z]/g;
  const unlistenLog = await listen<string>("site-template-log", (e) => {
    templateLogs.value.push(e.payload.replace(ansiRe, ""));
  });
  const unlistenProgress = await listen<TemplateProgress>("site-template-progress", (e) => {
    templateProgress.value = e.payload;
  });
  const labelMap: Record<string, string> = {
    blank: "空白", wordpress: "WordPress", laravel: "Laravel",
    thinkphp: "ThinkPHP", webman: "Webman",
  };
  const tplLabel = labelMap[template] || template;
  let success = false;
  try {
    await invoke("init_site_template", { targetDir, template });
    templateProgress.value = { phase: "done" };
    success = true;
    // 用户关掉了 modal 去做别的事 → 通过 toast 告知完成
    if (!showTemplateModal.value) {
      toast.success(`${tplLabel} 模板已安装完成`);
    }
  } catch (e) {
    const msg = String(e);
    templateLogs.value.push(`✗ ${msg}`);
    templateFailed.value = true; // 标记失败 → modal 显示 重试/换镜像 按钮
    syncCurrentMirror(); // 同步下拉显示当前镜像，方便用户选别的
    if (msg.includes("目录非空")) {
      showError(`模板初始化失败：${targetDir} 已有内容。请先清空该目录或换个新目录再创建。`);
    } else if (!showTemplateModal.value) {
      // modal 已关 → toast 兜底，让用户知道后台失败了
      showError(`${tplLabel} 模板安装失败：${msg}`);
    } else {
      showError("模板初始化失败: " + msg);
    }
  } finally {
    templateBusy.value = false;
    if (templateTimer !== null) { clearInterval(templateTimer); templateTimer = null; }
    unlistenLog();
    unlistenProgress();
  }
  return success;
}

async function confirmTakeover() {
  showTakeover.value = false;
  await doSaveVhost();
}

function deleteVhost(vh: VhostInfo) {
  pendingDelete.value = vh;
}

async function confirmDelete() {
  const vh = pendingDelete.value;
  if (!vh) return;
  busy.value = true;
  try {
    vhosts.value = await invoke("delete_vhost", { id: vh.id });
    showSuccess("站点已删除，已自动 reload Web 服务器");
  } catch (e) {
    showError("删除失败: " + e);
  } finally {
    busy.value = false;
    pendingDelete.value = null;
  }
}

async function browseFolder() {
  // 编辑/再次浏览时让 dialog 默认就跳到当前 document_root（或 wwwRoot 兜底）
  const s = await open({
    directory: true,
    multiple: false,
    defaultPath: form.value.document_root || wwwRoot.value || undefined,
  });
  if (s) { form.value.document_root = s as string; docRootTouched.value = true; }
}
async function browseFile(field: "ssl_cert" | "ssl_key") { const s = await open({ directory: false, multiple: false, filters: [{ name: "证书", extensions: ["pem","crt","key","cer"] }] }); if (s) form.value[field] = s as string; }

const sslBusy = ref(false);
async function autoGenCert() {
  if (sslBusy.value) return;
  if (!form.value.server_name.trim()) {
    showError("请先在基础配置里填 server_name（域名）");
    modalTab.value = "basic";
    return;
  }
  sslBusy.value = true;
  try {
    const aliases = form.value.aliases.split(/\s+/).filter(Boolean);
    const res = await invoke<{ cert_path: string; key_path: string }>(
      "generate_self_signed_cert",
      { serverName: form.value.server_name, aliases }
    );
    form.value.ssl_cert = res.cert_path;
    form.value.ssl_key = res.key_path;
    showSuccess("自签证书生成成功，保存站点即生效");
  } catch (e) {
    showError("生成失败: " + e);
  } finally {
    sslBusy.value = false;
  }
}

const filteredVhosts = computed(() => {
  if (!searchQuery.value.trim()) return vhosts.value;
  const q = searchQuery.value.toLowerCase();
  return vhosts.value.filter(v =>
    v.server_name.toLowerCase().includes(q) ||
    v.document_root.toLowerCase().includes(q) ||
    (v.php_version || "").toLowerCase().includes(q) ||
    v.aliases.some(a => a.toLowerCase().includes(q))
  );
});

// Grouped view: 自建/独立站点优先（用户自己创建的更常用），PHPStudy 站点（导入的）放下面
interface VhostGroup { key: string; label: string; items: VhostInfo[]; }
const groupedVhosts = computed<VhostGroup[]>(() => {
  const phpstudy = filteredVhosts.value.filter(v => v.source === "phpstudy");
  const custom = filteredVhosts.value.filter(v => v.source !== "phpstudy");
  const out: VhostGroup[] = [];
  if (custom.length) out.push({ key: "custom", label: "自建站点", items: custom });
  if (phpstudy.length) out.push({ key: "phpstudy", label: "PHPStudy 站点", items: phpstudy });
  return out;
});

/// 把 vhost 存的 php_version（"php855nts" 这种内部 id）反查成 { v: "8.5.5", variant: "nts" }。
/// 优先用后端 phpVersions 里的 label（权威格式），找不到再 fallback 解析。
function phpDisplay(raw: string | null): { v: string; variant: string } | null {
  if (!raw) return null;
  const found = phpVersions.value.find((p) => p.version === raw);
  if (found) {
    // label 形如 "PHP 8.5.5" 或 "PHP 8.5.5 (nts)"
    const m = found.label.match(/^PHP\s+(\S+)(?:\s+\((\S+)\))?/);
    if (m) return { v: m[1], variant: (m[2] || "").toLowerCase() };
  }
  // Fallback：vhost 引用了已删除/外部的 PHP 版本，直接按 "php{digits}{variant}" 解
  const m = raw.match(/^php(\d+)(nts|ts)?$/i);
  if (m) {
    const d = m[1];
    const v =
      d.length === 3
        ? `${d[0]}.${d[1]}.${d[2]}`
        : d.length >= 4
          ? `${d[0]}.${d[1]}.${d.slice(2)}`
          : d;
    return { v, variant: (m[2] || "").toLowerCase() };
  }
  return { v: raw, variant: "" };
}

function sourceBadge(v: VhostInfo): { text: string; color: string } | null {
  if (v.source === "phpstudy") return { text: "PS", color: "var(--color-purple)" };
  if (v.source === "standalone") return { text: "独立", color: "var(--color-cyan)" };
  return null; // custom: no badge, it's the default
}

function isExpired(vh: VhostInfo): boolean {
  if (!vh.expires_at) return false;
  const today = new Date().toISOString().slice(0, 10);
  return vh.expires_at.replace(/\//g, "-") <= today;
}

let expiryTimer: number | null = null;
async function checkExpired() {
  try { vhosts.value = await invoke("check_expired_vhosts"); } catch {}
}

let unlistenServicesChanged: UnlistenFn | null = null;

onMounted(async () => {
  loadVhosts(); loadPhpVersions(); loadWwwRoot();
  checkWebServer();
  expiryTimer = window.setInterval(checkExpired, 60000); // Check every 60s
  // 装包 / 切换 PHPStudy 路径后后端 emit services-changed
  // → 自动刷新 web server 检测、PHP 版本下拉、vhost 列表（旧路径 PS vhost 会消失）
  unlistenServicesChanged = await listen("services-changed", () => {
    checkWebServer();
    loadPhpVersions();
    loadVhosts();
  });
});
onUnmounted(() => {
  if (expiryTimer) clearInterval(expiryTimer);
  if (unlistenServicesChanged) unlistenServicesChanged();
});
</script>

<template>
  <div class="max-w-[960px]">
    <!-- 引导：未装任何 Web 服务器 → 点跳到软件商店 -->
    <div v-if="hasWebServer === false" class="card mb-3 flex items-center gap-3"
         style="border-left: 3px solid var(--color-warning); background: var(--bg-secondary)">
      <AlertCircle :size="20" style="color: var(--color-warning); flex-shrink: 0" />
      <div class="flex-1">
        <div class="text-[14px] font-semibold mb-0.5">还没装 Web 服务器</div>
        <div class="text-[12px]" style="color: var(--text-muted)">
          创建网站需要 Nginx 或 Apache。点右侧按钮去「软件商店」一键安装。
        </div>
      </div>
      <button class="btn btn-primary btn-sm" @click="router.push('/store')">去软件商店</button>
    </div>

    <!-- Header -->
    <div class="flex items-center justify-end mb-3">
      <div class="flex items-center gap-2">
        <input class="input" style="width: 200px" v-model="searchQuery" placeholder="搜索站点..." />
        <button class="btn btn-success btn-sm" @click="openCreate"
                :disabled="busy || hasWebServer === false"
                :title="hasWebServer === false ? '需要先安装 Nginx 或 Apache' : ''">+ 新建站点</button>
      </div>
    </div>

    <!-- Floating toast (per-row position, kept for expires-soon etc.) -->
    <div v-if="floatMsg" class="float-toast" :class="floatType">{{ floatMsg }}</div>

    <!-- ==================== Modal ==================== -->
    <div v-if="showForm" class="modal-overlay">
      <div class="modal-content">
        <div class="flex items-center justify-between mb-3">
          <div class="text-base font-semibold">{{ editingId ? "编辑站点" : "新建站点" }}</div>
          <div class="modal-actions !mt-0">
            <button class="btn btn-secondary btn-sm" @click="cancelForm" :disabled="busy">取消</button>
            <button class="btn btn-success btn-sm" @click="saveVhost" :disabled="busy">{{ busy ? "保存中..." : "保存" }}</button>
          </div>
        </div>

        <!-- Modal tabs -->
        <div class="flex gap-0 border-b mb-3" style="border-color: var(--border-color)">
          <button v-for="t in ([{k:'basic',l:'基础配置'},{k:'rewrite',l:'伪静态'},{k:'ssl',l:'SSL 与高级'}] as const)" :key="t.k"
            class="modal-tab" :class="{ active: modalTab === t.k }" @click="modalTab = t.k">{{ t.l }}</button>
        </div>

        <!-- Tab: 基础配置 -->
        <div v-show="modalTab === 'basic'" class="grid grid-cols-2 gap-3">
          <div class="fg"><label>域名 <span style="color:var(--color-danger)">*</span></label><input class="input" v-model="form.server_name" @input="onServerNameInput" placeholder="example.test" /></div>
          <div class="fg"><label>端口</label><input class="input" type="number" v-model.number="form.listen_port" min="1" max="65535" /></div>
          <div class="fg full"><label>域名别名 <span style="color:var(--text-muted);font-weight:400">多个用空格分隔</span></label><input class="input" v-model="form.aliases" placeholder="www.example.test" /></div>
          <div class="fg full"><label>网站目录 <span style="color:var(--color-danger)">*</span></label><div class="flex gap-1.5"><input class="input flex-1" v-model="form.document_root" @input="docRootTouched = true" :placeholder="wwwRoot ? wwwRoot + '/mysite' : '/path/to/site'" /><button class="btn btn-secondary btn-sm" @click="browseFolder">浏览</button></div></div>
          <div class="fg"><label>PHP 版本</label><SelectMenu v-model="form.php_version" :options="phpVersionOptions" full-width trigger-class="input" /></div>
          <div class="fg"><label>默认首页</label><input class="input" v-model="form.index_files" placeholder="index.php index.html" /></div>
          <div class="fg"><label>到期日期 <span style="color:var(--text-muted);font-weight:400">留空永不过期</span></label><input class="input" type="date" v-model="form.expires_at" /></div>
          <div v-if="!editingId" class="fg"><label>初始化模板 <span style="color:var(--text-muted);font-weight:400">仅在新建空目录时生效</span></label><SelectMenu v-model="initTemplate" :options="templateOptions" full-width trigger-class="input" /></div>
          <div class="fg-toggle"><span class="text-[13px] font-medium" style="color:var(--text-secondary)">同步 Hosts</span><label class="toggle-wrap"><input type="checkbox" v-model="form.sync_hosts" /><span class="toggle-slider"></span></label></div>
        </div>

        <!-- Tab: 伪静态 -->
        <div v-show="modalTab === 'rewrite'" class="grid grid-cols-2 gap-3">
          <div class="fg"><label>伪静态预设</label><SelectMenu :model-value="rewritePreset" :options="rewritePresetOptions" full-width trigger-class="input" @update:modelValue="onRewritePresetChange(String($event))" /></div>
          <div class="fg"><label>目录浏览</label><SelectMenu v-model="form.autoindex" :options="boolOptions" full-width trigger-class="input" /></div>
          <div class="fg full"><label>伪静态规则 <span style="color:var(--text-muted);font-weight:400">选择预设自动填充，可手动修改</span></label><textarea class="input resize-y font-mono text-[13px] min-h-[180px]" v-model="form.rewrite_rule" rows="5" spellcheck="false" @keydown="onTextareaTab" placeholder="try_files $uri $uri/ /index.php?$query_string;"></textarea></div>
        </div>

        <!-- Tab: SSL 与高级 -->
        <div v-show="modalTab === 'ssl'" class="grid grid-cols-2 gap-3">
          <div class="fg full">
            <div class="flex items-center justify-between mb-1">
              <label class="!mb-0">SSL 证书</label>
              <button class="btn btn-primary btn-sm" :disabled="sslBusy" @click="autoGenCert">
                {{ sslBusy ? '生成中...' : '🔒 一键生成 HTTPS 证书' }}
              </button>
            </div>
            <div class="text-[13px]" style="color: var(--text-muted)">
              首次签发会自动创建本地 CA 并安装到当前用户证书库（不需要管理员权限）。浏览器直接显示绿锁，不弹"不安全"警告。
            </div>
          </div>
          <div class="fg"><label>证书路径</label><div class="flex gap-1.5"><input class="input flex-1" v-model="form.ssl_cert" placeholder="*.pem / *.crt" /><button class="btn btn-secondary btn-sm" @click="browseFile('ssl_cert')">浏览</button></div></div>
          <div class="fg"><label>密钥路径</label><div class="flex gap-1.5"><input class="input flex-1" v-model="form.ssl_key" placeholder="*.key" /><button class="btn btn-secondary btn-sm" @click="browseFile('ssl_key')">浏览</button></div></div>
          <div class="fg"><label>强制 HTTPS</label><SelectMenu v-model="form.force_https" :options="boolOptions" full-width trigger-class="input" /></div>
          <div class="fg"><label>日志路径</label><input class="input" v-model="form.access_log" placeholder="留空使用默认" /></div>
          <div class="fg full"><label>自定义 Nginx 指令</label><textarea class="input resize-y font-mono text-[13px] min-h-[140px]" v-model="form.custom_directives" rows="4" spellcheck="false" @keydown="onTextareaTab" placeholder="proxy_pass http://127.0.0.1:3000;"></textarea></div>
        </div>
      </div>
    </div>

    <!-- ==================== Raw Nginx Conf Edit ==================== -->
    <div v-if="showConfModal" class="modal-overlay" @click.self="showConfModal = false">
      <div class="modal-content" style="width: 860px">
        <div class="flex items-center justify-between mb-3">
          <div>
            <div class="text-base font-semibold">Nginx 配置：{{ confEditingDomain }}</div>
            <div v-if="confPath" class="text-[12px] mt-1" style="color: var(--text-muted); font-family: var(--font-mono); word-break: break-all">{{ confPath }}</div>
          </div>
          <div class="flex gap-2">
            <button class="btn btn-secondary btn-sm" :disabled="confSaving" @click="showConfModal = false">取消</button>
            <button class="btn btn-success btn-sm" :disabled="confSaving" @click="saveConfEdit">{{ confSaving ? '保存中…' : '保存并 reload' }}</button>
          </div>
        </div>
        <textarea class="input font-mono text-[13px] w-full"
                  v-model="confContent"
                  spellcheck="false"
                  @keydown="onTextareaTab"
                  style="min-height: 50vh; max-height: 60vh; resize: vertical; line-height: 1.5"></textarea>
        <div class="text-[12px] mt-2" style="color: var(--text-muted)">
          ⚠️ 修改后会自动 reload nginx。语法错误会让 nginx reload 失败。<br />
          再次点击「✏️ 编辑」表单保存会按表单内容覆盖此文件。
        </div>
      </div>
    </div>

    <!-- ==================== Site Template Init Progress ==================== -->
    <div v-if="showTemplateModal" class="modal-overlay" @click.self="showTemplateModal = false">
      <div class="modal-content" style="width: 720px">
        <div class="flex items-center justify-between mb-2">
          <div class="text-base font-semibold">
            {{ templateBusy ? '正在初始化站点…' : (templateFailed ? '初始化失败' : '初始化完成') }}
          </div>
          <div class="flex gap-2 flex-wrap justify-end">
            <!-- 运行中：停止 -->
            <button v-if="templateBusy"
                    class="btn btn-danger btn-sm"
                    :disabled="templateCancelBusy"
                    @click="cancelTemplate">
              {{ templateCancelBusy ? '停止中...' : '⏹ 停止' }}
            </button>
            <!-- 失败后：换镜像（下拉手动选） + 重试 -->
            <template v-if="!templateBusy && templateFailed">
              <SelectMenu
                :model-value="templateMirrorPick"
                :options="templateMirrorOptions"
                trigger-class="btn btn-secondary btn-sm"
                placeholder="🔄 换镜像源"
                @update:modelValue="onMirrorPicked"
              />
              <button class="btn btn-primary btn-sm" @click="retryTemplate" title="用当前镜像重试">
                🔁 重试
              </button>
            </template>
            <button class="btn btn-secondary btn-sm" :disabled="!templateLogs.length" @click="copyTemplateLogs">复制日志</button>
            <button class="btn btn-secondary btn-sm" @click="showTemplateModal = false">{{ templateBusy ? '后台运行' : '关闭' }}</button>
          </div>
        </div>
        <!-- 状态条：阶段标签 + 进度条 + 已用时间 -->
        <div class="flex items-center gap-3 py-2 mb-2 border-b" style="border-color: var(--border-color)">
          <span v-if="templateBusy" class="animate-spin inline-block w-3 h-3 rounded-full border-2 shrink-0" style="border-color: var(--color-blue) transparent var(--color-blue) transparent"></span>
          <span v-else class="inline-block w-3 h-3 shrink-0" style="color: var(--color-success-light)">✓</span>
          <span class="text-[13px]" style="color: var(--text-secondary); min-width: 56px">{{ phaseLabel(templateProgress, templateBusy) }}</span>
          <div v-if="templateProgress?.current !== undefined && templateProgress?.total" class="flex-1 h-1.5 rounded-full overflow-hidden" style="background: var(--bg-tertiary)">
            <div class="h-full transition-all duration-300" :style="{ width: progressPercent(templateProgress) + '%', background: 'var(--color-blue)' }"></div>
          </div>
          <div v-else class="flex-1"></div>
          <span class="font-mono text-[12px]" style="color: var(--text-secondary)">{{ progressText(templateProgress) }}</span>
          <span class="font-mono text-[12px]" style="color: var(--text-muted)">· {{ templateElapsed }}s</span>
        </div>
        <pre class="font-mono text-[12px] w-full" style="background: var(--bg-tertiary); border: 1px solid var(--border-color); border-radius: 6px; padding: 12px; min-height: 50vh; max-height: 60vh; overflow-y: auto; white-space: pre-wrap; user-select: text; color: var(--text-secondary)">{{ templateLogs.join('\n') || '准备中...' }}</pre>
      </div>
    </div>

    <!-- ==================== Takeover Confirm ==================== -->
    <ConfirmDialog
      :open="showTakeover"
      title="接管 PHPStudy 站点"
      confirm-text="确认接管"
      :busy="busy"
      @confirm="confirmTakeover"
      @cancel="showTakeover = false"
    >
      保存后，<b>{{ form.server_name }}</b> 将由 NaxOne 接管管理。之后：
      <ul style="margin: 8px 0 0 18px; padding: 0; list-style: disc">
        <li>在 NaxOne 内的修改会写入 nginx 配置并自动 reload</li>
        <li>在 PHPStudy 软件内对该站点的修改可能被 NaxOne 下次保存覆盖</li>
        <li>站点会从「PHPSTUDY 站点」分组移到「自建站点」分组</li>
      </ul>
    </ConfirmDialog>

    <!-- ==================== Delete Confirm ==================== -->
    <ConfirmDialog
      :open="!!pendingDelete"
      title="删除站点"
      variant="danger"
      confirm-text="确认删除"
      :busy="busy"
      @confirm="confirmDelete"
      @cancel="pendingDelete = null"
    >
      <template v-if="pendingDelete">
        确认删除站点 <b>{{ pendingDelete.server_name }}:{{ pendingDelete.listen_port }}</b>？
        <div style="margin-top: 10px; color: var(--text-muted); font-size: 13px">
          将同时删除 Nginx + Apache 双向配置文件和 hosts 条目。<b style="color: var(--color-danger-hover)">此操作不可恢复。</b>
        </div>
        <div style="margin-top: 6px; color: var(--text-muted); font-size: 13px">
          站点目录文件不会被删除，仅清理服务器配置。
        </div>
      </template>
    </ConfirmDialog>

    <!-- ==================== List ==================== -->
    <div v-if="filteredVhosts.length === 0 && !showForm" class="text-center py-16 text-content-secondary">
      <p>暂无网站</p>
      <p class="mt-2 text-[16px] text-content-muted">点击上方按钮创建第一个站点</p>
    </div>

    <template v-for="g in groupedVhosts" :key="g.key">
    <div class="flex items-center gap-2 px-1 mt-3 mb-1.5 text-[13px] font-semibold uppercase tracking-widest"
         style="color: var(--text-muted)">
      <span>{{ g.label }}</span>
      <span class="px-1.5 py-px rounded text-[13px] font-mono" style="background: var(--bg-tertiary)">{{ g.items.length }}</span>
    </div>
    <div class="list-card mb-2">
      <!-- Header -->
      <div class="flex items-center px-5 py-2.5 text-[13px] font-semibold uppercase tracking-widest"
           style="color: var(--text-muted)">
        <span class="w-[200px]">域名</span>
        <span class="w-20">端口</span>
        <span class="flex-1 min-w-0">网站目录</span>
        <span class="w-20 text-center">PHP</span>
        <span class="w-20 text-center">到期</span>
        <span class="w-14 text-center">状态</span>
        <span class="w-48 text-right">操作</span>
      </div>
      <!-- Rows -->
      <div v-for="vh in g.items" :key="vh.id" class="group list-row gap-0">
        <div class="w-[200px] shrink-0 min-w-0">
          <div class="flex items-center gap-1.5">
            <span class="text-[16px] font-medium truncate">{{ vh.server_name }}</span>
            <span v-if="vh.has_ssl" class="text-[13px] px-1.5 py-px rounded font-medium shrink-0"
                  style="background: rgba(34,197,94,0.15); color: var(--color-success-light)">SSL</span>
            <span v-if="sourceBadge(vh)" class="text-[13px] px-1.5 py-px rounded font-medium shrink-0"
                  :style="{ background: `${sourceBadge(vh)!.color}22`, color: sourceBadge(vh)!.color }">{{ sourceBadge(vh)!.text }}</span>
          </div>
          <div v-if="vh.aliases.length > 0" class="text-[13px] mt-0.5 truncate" style="color: var(--text-muted)">{{ vh.aliases.join(", ") }}</div>
        </div>
        <div class="w-20 shrink-0 text-[16px] font-mono" style="color: var(--text-muted)">{{ vh.has_ssl ? `${vh.listen_port}/443` : vh.listen_port }}</div>
        <div class="flex-1 min-w-0 text-[16px] truncate pr-3" style="color: var(--text-secondary)" :title="vh.document_root">{{ vh.document_root }}</div>
        <div class="w-20 shrink-0 text-center">
          <template v-if="phpDisplay(vh.php_version)">
            <span class="text-[13px] font-mono" style="color: var(--text-secondary)">{{ phpDisplay(vh.php_version)!.v }}</span>
            <span v-if="phpDisplay(vh.php_version)!.variant" class="text-[13px] ml-0.5" style="color: var(--text-muted)">{{ phpDisplay(vh.php_version)!.variant }}</span>
          </template>
          <span v-else class="text-[13px]" style="color: var(--text-muted)">-</span>
        </div>
        <div class="w-20 shrink-0 text-center text-[13px]" :style="{ color: isExpired(vh) ? 'var(--color-danger)' : 'var(--text-muted)' }">{{ vh.expires_at ? (isExpired(vh) ? '已过期' : formatDate(vh.expires_at)) : '永久' }}</div>
        <div class="w-14 shrink-0 flex justify-center">
          <label class="toggle-wrap" @click.prevent="toggleVhost(vh)">
            <input type="checkbox" :checked="vh.enabled" />
            <span class="toggle-slider"></span>
          </label>
        </div>
        <div class="w-48 shrink-0 flex items-center gap-1 justify-end">
            <button class="btn btn-secondary btn-sm !px-2 transition-opacity" @click="openEdit(vh)" :disabled="busy" title="编辑"><Pencil :size="14" /></button>
            <button class="btn btn-secondary btn-sm !px-2 transition-opacity" @click="openConfEdit(vh)" :disabled="busy" title="查看/编辑 nginx 配置"><FileText :size="14" /></button>
            <button class="btn btn-secondary btn-sm !px-2 transition-opacity" @click="openSite(vh); showFloat('已在浏览器打开')" title="在浏览器打开"><ExternalLink :size="14" /></button>
            <button class="btn btn-secondary btn-sm !px-2 transition-opacity" @click="openDir(vh); showFloat('已打开目录')" title="打开目录"><FolderOpen :size="14" /></button>
            <button class="btn btn-secondary btn-sm !px-2 transition-opacity hover:!text-red-400" @click="deleteVhost(vh)" :disabled="busy" title="删除"><Trash2 :size="14" /></button>
        </div>
      </div>
    </div>
    </template>
  </div>
</template>
