// 真正的 GUI 自动化：通过 CDP 模拟用户点击/输入 DOM 元素，验证 retryTemplate
// 整条前端代码路径，包括我加的 cleanup 调用。
//
// 流程：
//   1. invoke 前置准备（设错误镜像 + 创建/清理测试目录）
//   2. **点击侧栏「网站」**（DOM .click()）
//   3. **点击「+ 新建站点」按钮**
//   4. **填表单 input 框**（dispatch input event）
//   5. **点保存**
//   6. 等失败 modal 出现
//   7. **点 modal 内「重试」按钮**
//   8. 验证 templateLogs 出现"🧹 已清理"
//   9. 关 modal + 清理测试 vhost
import WebSocket from "ws";
import fs from "node:fs";
import path from "node:path";

const targets = await (await fetch("http://127.0.0.1:9222/json")).json();
const t = targets.find((x) => x.type === "page" && x.url.startsWith("http://localhost:5173"));
if (!t) { console.log("✗ 未找到 webview"); process.exit(1); }
const ws = new WebSocket(t.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
ws.on("message", (data) => {
  const m = JSON.parse(data.toString());
  if (m.id && pending.has(m.id)) {
    const { resolve, reject } = pending.get(m.id);
    pending.delete(m.id);
    if (m.error) reject(new Error(m.error.message || JSON.stringify(m.error)));
    else resolve(m.result);
  }
});
await new Promise((r) => ws.once("open", r));

function send(method, params = {}) {
  const myId = ++id;
  return new Promise((resolve, reject) => {
    pending.set(myId, { resolve, reject });
    ws.send(JSON.stringify({ id: myId, method, params }));
    setTimeout(() => { if (pending.has(myId)) { pending.delete(myId); reject(new Error(`timeout: ${method}`)); } }, 30000);
  });
}

async function evalJs(expr) {
  const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error("JS exception: " + JSON.stringify(r.exceptionDetails));
  return r.result.value;
}

async function invoke(cmd, args = {}) {
  const argsJson = JSON.stringify(args);
  const v = await evalJs(`
    (async () => {
      try {
        const inv = window.__TAURI_INTERNALS__?.invoke || window.__TAURI__?.core?.invoke;
        if (!inv) return { __err: "no invoke" };
        const r = await inv("${cmd}", ${argsJson});
        return { __ok: r };
      } catch (e) { return { __err: String(e?.message || e) }; }
    })()`);
  if (v.__err) throw new Error(v.__err);
  return v.__ok;
}

async function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

async function waitFor(predicateExpr, timeoutMs = 10000, intervalMs = 200) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const ok = await evalJs(`(function(){ try { return Boolean(${predicateExpr}); } catch { return false; } })()`);
    if (ok) return true;
    await sleep(intervalMs);
  }
  return false;
}

// ============ 前置 ============
console.log("=== 前置 ===");
await evalJs(`new Promise(r => { const t = () => document.querySelector("main") ? r(true) : setTimeout(t, 100); t(); })`);
console.log("✓ Vue 就绪");

const TEST_HOST = "gui-retry-test.invalid";
const TEST_PORT = 18802;
const TEST_DIR = "D:/temp/naxone-gui-retry-test";
const TEST_ID = `${TEST_HOST}_${TEST_PORT}`;

// 清旧 vhost 如果有
try { await invoke("delete_vhost", { id: TEST_ID }); console.log(`  清旧 vhost: ${TEST_ID}`); } catch {}

// 准备测试目录
fs.mkdirSync(TEST_DIR, { recursive: true });
for (const f of fs.readdirSync(TEST_DIR)) {
  const p = path.join(TEST_DIR, f);
  if (fs.statSync(p).isDirectory()) fs.rmSync(p, { recursive: true, force: true });
  else fs.unlinkSync(p);
}
// 塞模拟 composer 残留（这次直接塞而不是真跑 composer，省时间）
fs.writeFileSync(path.join(TEST_DIR, "composer.json"), '{"name":"laravel/laravel"}');
fs.writeFileSync(path.join(TEST_DIR, "composer.lock"), '{}');
fs.mkdirSync(path.join(TEST_DIR, "vendor"), { recursive: true });
fs.writeFileSync(path.join(TEST_DIR, "vendor/autoload.php"), "<?php\n");
console.log(`✓ 测试目录就绪: ${fs.readdirSync(TEST_DIR)}`);

// 直接通过 IPC 创建测试 vhost（sync_hosts=false 避 UAC）
await invoke("create_vhost", {
  req: {
    server_name: TEST_HOST, aliases: "", listen_port: TEST_PORT,
    document_root: TEST_DIR, php_version: null, index_files: "index.php index.html",
    rewrite_rule: "", autoindex: false, ssl_cert: null, ssl_key: null,
    force_https: false, custom_directives: null, access_log: null,
    sync_hosts: false, expires_at: "",
  },
});
console.log(`✓ 已建测试 vhost ${TEST_ID}`);

// ============ 真 GUI：切到 网站 tab ============
console.log("\n=== GUI: 切到 网站 tab ===");
await evalJs(`
  (function(){
    const items = Array.from(document.querySelectorAll("aside nav > div"));
    const w = items.find(el => el.textContent.includes("网站"));
    if (!w) throw new Error("没找到侧栏 网站 菜单");
    w.click();
  })()
`);
await sleep(800);
const onVhostsPage = await waitFor(`document.querySelector('main input[placeholder*="搜索"]') !== null`);
console.log(`✓ 网站页加载完成: ${onVhostsPage}`);

// ============ 验证测试 vhost 出现在列表（说明前端已 fetch）============
const vhostInList = await waitFor(`document.querySelector('main').textContent.includes("${TEST_HOST}")`, 5000);
console.log(`✓ 测试 vhost 在列表: ${vhostInList}`);

// ============ 真 GUI：直接调 saveVhost 触发的代码路径 ============
// 实际场景应该是用户点编辑 → 改模板 → 保存。我们简化：直接用 retryTemplate 的入口 ref。
// 但 retryTemplate 需要 templateLastTarget / templateLastTpl 已设。
// 所以**真正合法**的 GUI 路径：
//   方法 A：点编辑 → modal → 选模板 → 保存（涉及表单很多输入）
//   方法 B：直接用 vhost 元数据触发 runTemplateInit（vue 内 ref）
// 我们用 **A 方案** 走真实路径。
//
// 但是！编辑现有 vhost 选模板会被前端代码"编辑模式不显示模板"拦截。所以正确路径：
// → 点 + 新建站点 → 填表单 → 选 Laravel → 保存（vhost 已存在会冲突）
//
// 为了走通真实 GUI 链路，先**删 testvhost**，让 saveVhost 走 create 分支：

// 简化：用 evaluate 直接调用 Vue 内部的 runTemplateInit + 失败 + 重试。
// 测的是 retryTemplate 的代码（含 cleanup 调用），不是 saveVhost UI 链。
console.log("\n=== GUI: 通过 modal 触发 runTemplateInit ===");

// 先 set 一个失败镜像，让 composer 必失败
try { await invoke("set_composer_repo", { url: "https://nonexistent-gui-test.invalid/composer/" }); } catch {}

// 触发 runTemplateInit：通过 evaluate 直接调函数（这是 vue 顶层 const 不暴露 window，做不到）。
// 走 alternative：模拟"重试"按钮被点击的最终状态。
// 关键状态：templateLastTarget / templateLastTpl / templateFailed = true → 用户点重试按钮

// 怎么让 templateLastTarget 等设置？只有 runTemplateInit 跑过才设。
// 那唯一办法：通过 UI 走一次新建站点表单 → 选模板 → 保存 → 触发 runTemplateInit。

// 流程：点 + 新建站点
console.log("  - 点击「+ 新建站点」");
await evalJs(`
  (function(){
    const btns = Array.from(document.querySelectorAll("main button"));
    const b = btns.find(b => b.textContent.includes("新建站点"));
    if (!b) throw new Error("没找到 新建站点 按钮");
    b.click();
  })()
`);
await sleep(500);
const formOpened = await waitFor(`document.querySelector('.modal-overlay input[placeholder="example.test"]') !== null`);
console.log(`  ✓ 新建站点 modal 打开: ${formOpened}`);

// 填表单（用合成 input event 触发 vue v-model）
const TEST_HOST2 = "gui-retry-2nd.invalid";
const TEST_PORT2 = 18803;
const TEST_ID2 = `${TEST_HOST2}_${TEST_PORT2}`;
const TEST_DIR2 = "D:/temp/naxone-gui-retry-2nd";
fs.mkdirSync(TEST_DIR2, { recursive: true });
// 先清空 + 塞模拟失败后残留 —— 让 composer 跳过到目录非空检查
for (const f of fs.readdirSync(TEST_DIR2)) {
  const p = path.join(TEST_DIR2, f);
  if (fs.statSync(p).isDirectory()) fs.rmSync(p, { recursive: true, force: true });
  else fs.unlinkSync(p);
}

console.log("  - 填表单（域名/端口/目录）");
await evalJs(`
  (function(){
    function setInputValue(sel, val) {
      const inp = document.querySelector(sel);
      if (!inp) throw new Error("input 不存在: " + sel);
      inp.focus();
      // vue 3 v-model 监听 input 事件
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
      setter.call(inp, val);
      inp.dispatchEvent(new Event("input", { bubbles: true }));
    }
    setInputValue('.modal-overlay input[placeholder="example.test"]', '${TEST_HOST2}');
    // 端口 input type=number
    setInputValue('.modal-overlay input[type="number"]', '${TEST_PORT2}');
    // 网站目录
    const dirInputs = document.querySelectorAll('.modal-overlay input.input');
    let docRootInp = null;
    for (const i of dirInputs) {
      if (i.placeholder && (i.placeholder.includes('mysite') || i.placeholder.includes('/path/to/site'))) {
        docRootInp = i; break;
      }
    }
    if (!docRootInp) {
      // fallback: 第 4 个 input 通常是目录
      docRootInp = dirInputs[3];
    }
    if (docRootInp) {
      docRootInp.focus();
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
      setter.call(docRootInp, '${TEST_DIR2}');
      docRootInp.dispatchEvent(new Event("input", { bubbles: true }));
    }
  })()
`);
await sleep(300);

// 选模板 → Laravel（SelectMenu 操作较复杂，简化：通过 invoke 在 vhost 创建后再装模板）
// 为了不卡在 SelectMenu，**先关 modal + 直接 invoke 走 runTemplateInit 等价物**
console.log("  - 取消 modal（避免点保存）");
await evalJs(`
  (function(){
    const cancelBtn = Array.from(document.querySelectorAll('.modal-overlay button')).find(b => b.textContent.trim() === '取消');
    if (cancelBtn) cancelBtn.click();
  })()
`);
await sleep(300);

// 重新用 IPC 直接创建 vhost #2
await invoke("create_vhost", {
  req: {
    server_name: TEST_HOST2, aliases: "", listen_port: TEST_PORT2,
    document_root: TEST_DIR2, php_version: null, index_files: "index.php index.html",
    rewrite_rule: "", autoindex: false, ssl_cert: null, ssl_key: null,
    force_https: false, custom_directives: null, access_log: null,
    sync_hosts: false, expires_at: "",
  },
});

// 塞模拟残留
fs.writeFileSync(path.join(TEST_DIR2, "composer.json"), '{"name":"laravel/laravel"}');
fs.writeFileSync(path.join(TEST_DIR2, "vendor"), ""); // 误把 vendor 写成文件了，改用 dir
fs.unlinkSync(path.join(TEST_DIR2, "vendor"));
fs.mkdirSync(path.join(TEST_DIR2, "vendor"), { recursive: true });
fs.writeFileSync(path.join(TEST_DIR2, "vendor/autoload.php"), "<?php\n");

console.log(`  ✓ 测试 vhost #2 + 模拟残留就绪: ${fs.readdirSync(TEST_DIR2)}`);

// ============ 关键：让 Vue 进入"装包失败"状态 ============
// 我们没法直接 invoke runTemplateInit（前端函数）但可以**直接调 init_site_template** 触发一次失败，
// 然后**手动设** templateFailed=true 等 vue ref，让 UI 显示重试 modal。
// 不可行 —— ref 是 vue 内部闭包。
//
// 替代方案：触发整个**新建模态保存** → 让 vue 自己跑 runTemplateInit → 失败 → 显示 modal。
// 但 vhost #2 已被创建，再次新建会冲突。换思路：删除 vhost #2 再走 UI 流程。

await invoke("delete_vhost", { id: TEST_ID2 });
fs.rmSync(TEST_DIR2, { recursive: true, force: true });
fs.mkdirSync(TEST_DIR2, { recursive: true });
// **关键**：预先塞残留进 TEST_DIR2，让第一次 init_site_template 报"目录非空"失败，
// 重试时 cleanup 必然清出 N 项 → 显示"🧹 已清理"。
// 真实场景是 composer 跑到一半失败留下残留，效果一样。
fs.writeFileSync(path.join(TEST_DIR2, "composer.json"), '{"name":"laravel/laravel"}');
fs.writeFileSync(path.join(TEST_DIR2, "composer.lock"), '{}');
fs.writeFileSync(path.join(TEST_DIR2, ".env"), "APP_KEY=test");
fs.writeFileSync(path.join(TEST_DIR2, "artisan"), "<?php\n");
fs.mkdirSync(path.join(TEST_DIR2, "vendor"), { recursive: true });
fs.writeFileSync(path.join(TEST_DIR2, "vendor/autoload.php"), "<?php\n");
fs.mkdirSync(path.join(TEST_DIR2, "app"), { recursive: true });
console.log(`  ✓ 删 vhost #2 + 重建目录 + 塞 ${fs.readdirSync(TEST_DIR2).length} 项模拟残留`);

// 重新打开 modal 走完整 UI 流程
console.log("\n=== 走完整 UI 流程：填表单 + 选模板 + 保存 ===");
await evalJs(`
  (function(){
    const btns = Array.from(document.querySelectorAll("main button"));
    const b = btns.find(b => b.textContent.includes("新建站点"));
    b.click();
  })()
`);
await sleep(500);

await evalJs(`
  (function(){
    function setInputValue(sel, val) {
      const inp = document.querySelector(sel);
      if (!inp) throw new Error("input 不存在: " + sel);
      inp.focus();
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
      setter.call(inp, val);
      inp.dispatchEvent(new Event("input", { bubbles: true }));
    }
    setInputValue('.modal-overlay input[placeholder="example.test"]', '${TEST_HOST2}');
    setInputValue('.modal-overlay input[type="number"]', '${TEST_PORT2}');
    // 找文档根目录 input：在 .fg.full label "网站目录" 下
    const labels = Array.from(document.querySelectorAll('.modal-overlay label'));
    const docLabel = labels.find(l => l.textContent.includes('网站目录'));
    if (docLabel) {
      const fg = docLabel.closest('.fg');
      const inp = fg ? fg.querySelector('input.input') : null;
      if (inp) {
        inp.focus();
        const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
        setter.call(inp, '${TEST_DIR2}');
        inp.dispatchEvent(new Event("input", { bubbles: true }));
      }
    }
  })()
`);
await sleep(300);
console.log("  ✓ 表单已填");

// 选 Laravel 模板（点 SelectMenu trigger → 点 Laravel 选项）
console.log("  - 选 Laravel 模板");
await evalJs(`
  (function(){
    // 找"初始化模板" label 下的 SelectMenu
    const labels = Array.from(document.querySelectorAll('.modal-overlay label'));
    const tplLabel = labels.find(l => l.textContent.includes('初始化模板'));
    if (!tplLabel) throw new Error('找不到 初始化模板 label');
    const fg = tplLabel.closest('.fg');
    const trigger = fg.querySelector('.rs-select-trigger');
    if (!trigger) throw new Error('找不到模板下拉触发器');
    trigger.click();
  })()
`);
await sleep(500);
// 点 Laravel
await evalJs(`
  (function(){
    const items = Array.from(document.querySelectorAll('.rs-select-option, [role="option"], .rs-select-item'));
    const laravel = items.find(el => el.textContent.includes('Laravel'));
    if (!laravel) {
      // 列所有可见 option
      const allOpts = Array.from(document.querySelectorAll('body *')).filter(el => {
        const cs = getComputedStyle(el);
        return cs.position === 'fixed' && el.textContent && el.textContent.includes('Laravel') && el.offsetParent !== null;
      });
      if (allOpts.length === 0) throw new Error('找不到 Laravel 选项');
      // 取最深层的（叶节点）
      let leaf = allOpts[0];
      for (const o of allOpts) { if (o.children.length === 0 || o.textContent.trim() === 'Laravel') leaf = o; }
      leaf.click();
      return 'leaf-click';
    }
    laravel.click();
    return 'option-click';
  })()
`);
await sleep(500);
console.log("  ✓ 已选 Laravel 模板");

// 点保存
console.log("  - 点保存");
await evalJs(`
  (function(){
    const btns = Array.from(document.querySelectorAll('.modal-overlay button'));
    const save = btns.find(b => b.textContent.trim() === '保存');
    if (!save) throw new Error('没找到保存按钮');
    save.click();
  })()
`);

// 等待装包 modal 出现 + 失败
console.log("  - 等装包失败...");
const failed = await waitFor(`
  document.querySelector('.modal-overlay .text-base.font-semibold') &&
  document.querySelector('.modal-overlay .text-base.font-semibold').textContent.includes('失败')
`, 60000, 500);
console.log(`  ✓ 装包失败 modal 已出现: ${failed}`);

if (!failed) {
  console.log("✗ 装包没失败/没出现失败 modal，看下当前 modal 状态:");
  const debugInfo = await evalJs(`
    (function(){
      const titles = Array.from(document.querySelectorAll('.modal-overlay .text-base.font-semibold')).map(el => el.textContent);
      const logs = Array.from(document.querySelectorAll('.modal-overlay pre, .modal-overlay code')).map(el => el.textContent.slice(0, 200));
      return { titles, logs };
    })()
  `);
  console.log(JSON.stringify(debugInfo, null, 2));
  ws.close();
  process.exit(1);
}

// 看 templateLogs 当前内容
const logsBeforeRetry = await evalJs(`
  Array.from(document.querySelectorAll('.modal-overlay pre, .modal-overlay code, .modal-overlay div')).map(el => el.textContent).filter(t => t.includes('✗') || t.includes('🧹') || t.includes('开始'))
`);
console.log(`\n  装包日志 (重试前): ${JSON.stringify(logsBeforeRetry.slice(-5))}`);

// ============ 核心：点击「重试」按钮 ============
console.log("\n=== 点击「重试」按钮 ===");
const retryClicked = await evalJs(`
  (function(){
    const btns = Array.from(document.querySelectorAll('.modal-overlay button'));
    const retry = btns.find(b => b.textContent.includes('重试'));
    if (!retry) {
      return { found: false, allBtns: btns.map(b => b.textContent.trim()) };
    }
    retry.click();
    return { found: true };
  })()
`);
if (!retryClicked.found) {
  console.log(`✗ 没找到重试按钮，当前 modal 所有按钮:`);
  console.log(JSON.stringify(retryClicked.allBtns, null, 2));
  ws.close();
  process.exit(1);
}
console.log("✓ 已点击重试按钮");

// 等 "🧹 已清理 N 项失败残留" 日志出现
const cleanupLogShown = await waitFor(`
  (function(){
    const modal = document.querySelector('.modal-overlay');
    if (!modal) return false;
    return modal.textContent.includes('已清理') && modal.textContent.includes('🧹');
  })()
`, 5000, 200);

console.log(`\n${cleanupLogShown ? "✓" : "✗"} 测试核心：重试触发 cleanup_template_dir → 日志显示"🧹 已清理"`);

// 抓最终日志
const finalLogs = await evalJs(`
  (function(){
    const modal = document.querySelector('.modal-overlay');
    if (!modal) return [];
    // 抓 modal 内所有文本节点的文本
    return modal.textContent.split('\\n').map(s => s.trim()).filter(s => s.includes('🧹') || s.includes('已清理') || s.includes('开始') || s.includes('✗') || s.includes('PHP:') || s.includes('Composer:'));
  })()
`);
console.log(`  完整日志摘录: ${JSON.stringify(finalLogs.slice(-8), null, 2)}`);

// ============ 收尾 ============
console.log("\n=== 收尾 ===");
// 关 modal（点关闭按钮）
await evalJs(`
  (function(){
    const btns = Array.from(document.querySelectorAll('.modal-overlay button'));
    const close = btns.find(b => b.textContent.trim() === '关闭' || b.textContent.includes('后台'));
    if (close) close.click();
  })()
`);
await sleep(500);

// 取消停止后台任务 + 删测试 vhost + 测试目录
try { await invoke("cancel_init_site_template"); } catch {}
try { await invoke("delete_vhost", { id: TEST_ID }); } catch {}
try { await invoke("delete_vhost", { id: TEST_ID2 }); } catch {}
try { fs.rmSync(TEST_DIR, { recursive: true, force: true }); } catch {}
try { fs.rmSync(TEST_DIR2, { recursive: true, force: true }); } catch {}
console.log("✓ 测试环境已清干净");

ws.close();

console.log("\n========== GUI 自动化测试总结 ==========");
const pass = formOpened && failed && retryClicked.found && cleanupLogShown;
console.log(`新建 modal 打开: ${formOpened ? "✓" : "✗"}`);
console.log(`装包失败 modal 出现: ${failed ? "✓" : "✗"}`);
console.log(`重试按钮可点击: ${retryClicked.found ? "✓" : "✗"}`);
console.log(`重试触发 cleanup（"🧹 已清理"显示）: ${cleanupLogShown ? "✓" : "✗"}`);
console.log(`\n整体: ${pass ? "✓ 通过" : "✗ 失败"}`);
process.exit(pass ? 0 : 1);
