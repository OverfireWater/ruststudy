// 端到端测试：模板装包失败重试 → cleanup_template_dir 自动清理 → 重试通过空目录校验。
// 使用 throw-away vhost，**不动**任何真实站点。
import WebSocket from "ws";
import fs from "node:fs";
import path from "node:path";

// 连 CDP
const targets = await (await fetch("http://127.0.0.1:9222/json")).json();
const t = targets.find((x) => x.type === "page" && x.url.startsWith("http://localhost:5173"));
if (!t) {
  console.log("✗ 未找到 NaxOne dev webview");
  process.exit(1);
}
const ws = new WebSocket(t.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
ws.on("message", (data) => {
  const msg = JSON.parse(data.toString());
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(msg.error.message || JSON.stringify(msg.error)));
    else resolve(msg.result);
  }
});
await new Promise((r) => ws.once("open", r));

function send(method, params = {}) {
  const myId = ++id;
  return new Promise((resolve, reject) => {
    pending.set(myId, { resolve, reject });
    ws.send(JSON.stringify({ id: myId, method, params }));
    setTimeout(() => {
      if (pending.has(myId)) {
        pending.delete(myId);
        reject(new Error(`timeout: ${method}`));
      }
    }, 30000);
  });
}

async function invoke(cmd, args = {}) {
  const argsJson = JSON.stringify(args).replace(/\\/g, "\\\\").replace(/'/g, "\\'");
  const expr = `
    (async () => {
      try {
        const inv = window.__TAURI_INTERNALS__?.invoke || window.__TAURI__?.core?.invoke;
        if (!inv) return { __err: "no invoke" };
        const r = await inv("${cmd}", ${argsJson});
        return { __ok: r };
      } catch (e) {
        return { __err: String(e?.message || e) };
      }
    })()
  `;
  const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error("JS exception: " + JSON.stringify(r.exceptionDetails));
  const v = r.result.value;
  if (v.__err) throw new Error(v.__err);
  return v.__ok;
}

// 等 Vue 就绪
await send("Runtime.evaluate", {
  expression: `new Promise(r => { const t = () => document.querySelector("main") ? r(true) : setTimeout(t, 100); t(); })`,
  awaitPromise: true,
});
console.log("✓ NaxOne 就绪");

const TEST_HOST = "cleanup-e2e-test.invalid";
const TEST_PORT = 18801;
const TEST_DIR = "D:/temp/naxone-cleanup-e2e-test";

// 清理之前可能残留的 throw-away vhost
async function tryDeleteVhost(id) {
  try { await invoke("delete_vhost", { id }); console.log(`  清旧 vhost: ${id}`); } catch {}
}
await tryDeleteVhost(`${TEST_HOST}_${TEST_PORT}`);

// 准备 throw-away 目录
fs.mkdirSync(TEST_DIR, { recursive: true });
// 清空（万一上次没清干净）
for (const f of fs.readdirSync(TEST_DIR)) {
  const p = path.join(TEST_DIR, f);
  if (fs.statSync(p).isDirectory()) fs.rmSync(p, { recursive: true, force: true });
  else fs.unlinkSync(p);
}
console.log(`✓ 测试目录已就绪: ${TEST_DIR}`);

// ============ Step 1: 创建 throw-away vhost（不写 hosts 避免 UAC）============
console.log("\n=== Step 1: 创建测试 vhost ===");
try {
  await invoke("create_vhost", {
    req: {
      server_name: TEST_HOST,
      aliases: "",
      listen_port: TEST_PORT,
      document_root: TEST_DIR,
      php_version: null, // webman 风格，不要 fastcgi
      index_files: "index.php index.html",
      rewrite_rule: "",
      autoindex: false,
      ssl_cert: null,
      ssl_key: null,
      force_https: false,
      custom_directives: null,
      access_log: null,
      sync_hosts: false, // 关键：不弹 UAC
      expires_at: "",
    },
  });
  console.log(`✓ 已创建 vhost: ${TEST_HOST}:${TEST_PORT} → ${TEST_DIR}`);
} catch (e) {
  console.log(`✗ 创建 vhost 失败: ${e}`);
  ws.close();
  process.exit(1);
}

const lsDir = () => fs.readdirSync(TEST_DIR).sort();
console.log(`  create_vhost 后目录: ${JSON.stringify(lsDir())}`);

// ============ Step 2: 塞模拟 composer 残留（模拟装包失败后的状态）============
console.log("\n=== Step 2: 塞模拟 composer create-project 残留 ===");
fs.writeFileSync(path.join(TEST_DIR, "composer.json"), '{"name":"laravel/laravel"}');
fs.writeFileSync(path.join(TEST_DIR, "composer.lock"), '{"_readme":["test"]}');
fs.writeFileSync(path.join(TEST_DIR, ".env"), "APP_KEY=test");
fs.writeFileSync(path.join(TEST_DIR, "artisan"), "#!/usr/bin/env php\n<?php\n");
fs.mkdirSync(path.join(TEST_DIR, "vendor", "laravel", "framework"), { recursive: true });
fs.writeFileSync(path.join(TEST_DIR, "vendor", "autoload.php"), "<?php\n");
fs.mkdirSync(path.join(TEST_DIR, "app", "Http"), { recursive: true });
fs.writeFileSync(path.join(TEST_DIR, "app", "Http", "Kernel.php"), "<?php\n");
console.log(`  塞入后目录: ${JSON.stringify(lsDir())}`);

// ============ Step 3: init_site_template 应失败"目录非空"（原 bug 行为，验证场景真实）============
console.log("\n=== Step 3: init_site_template 应报 '目录非空' ===");
let pass1 = false;
try {
  await invoke("init_site_template", { targetDir: TEST_DIR, template: "thinkphp" });
  console.log("✗ 测试 1 失败：装包居然没报错（说明白名单或校验逻辑变了）");
} catch (e) {
  if (String(e).includes("目录非空") || String(e).includes("请先清空")) {
    console.log(`✓ 测试 1 通过：init_site_template 拒绝 (${String(e).slice(0, 80)})`);
    pass1 = true;
  } else {
    console.log(`? 测试 1 :报错但不是预期 "目录非空"：${e}`);
  }
}

// ============ Step 4: 调 cleanup_template_dir 清理 ============
console.log("\n=== Step 4: cleanup_template_dir 清理残留 ===");
const cleanedCount = await invoke("cleanup_template_dir", { targetDir: TEST_DIR });
console.log(`✓ cleanup 返回: ${cleanedCount} 项`);
const after = lsDir();
console.log(`  清完后目录: ${JSON.stringify(after)}`);

// 注：sync_hosts=false 仍然会写 nginx.htaccess/.htaccess 吗？看 vhost_mgr 代码 —— 是的，
// 写 htaccess 只看 rewrite_rule 是否非空。我们传 rewrite_rule="" → 不写 htaccess。
// 所以期望 after = [] （目录全空）
const survived = after.filter((f) => !["nginx.htaccess", ".htaccess"].includes(f));
let pass2 = survived.length === 0;
console.log(`  非白名单残留: ${JSON.stringify(survived)}`);
console.log(`${pass2 ? "✓" : "✗"} 测试 2：cleanup 清空非白名单文件`);

// ============ Step 5: 验证 init_site_template 现在能通过空目录校验 ============
// 注：composer 因为镜像源不通会失败，但**不是因为"目录非空"** —— 这是我们要验证的
console.log("\n=== Step 5: 再次 init_site_template 应通过空目录校验 ===");
// 先 set 一个不通的镜像源让 composer 必失败但不挂太久
try {
  await invoke("set_composer_repo", { url: "https://nonexistent-cleanup-test.invalid/composer/" });
} catch {}

let pass3 = false;
let initErr = "";
try {
  // thinkphp 走 composer，必失败
  // 设定较短超时（30s），但 composer DNS 解析 invalid 应该立即失败
  const initPromise = invoke("init_site_template", { targetDir: TEST_DIR, template: "thinkphp" });
  const timeoutPromise = new Promise((_, rej) => setTimeout(() => rej(new Error("e2e timeout 30s")), 30000));
  await Promise.race([initPromise, timeoutPromise]);
  // 这里通常不会到 —— composer 必失败
  console.log("? 装包居然成功了（不应该，镜像源不通）");
  pass3 = true; // 这种情况也算通过
} catch (e) {
  initErr = String(e);
  if (initErr.includes("目录非空") || initErr.includes("请先清空")) {
    console.log(`✗ 测试 3 失败：cleanup 后仍报 '目录非空' —— retry 链路坏了！`);
  } else {
    // 任何**别的**错（composer 失败、网络错、超时）都算通过 —— 目录校验已经跨过
    console.log(`✓ 测试 3 通过：cleanup 后入口校验通过，失败原因不是 '目录非空'`);
    console.log(`  实际错误（非目录非空）: ${initErr.slice(0, 120)}`);
    pass3 = true;
  }
}

// ============ Step 6: cleanup 收尾 + 删测试 vhost ============
console.log("\n=== Step 6: 清理测试残留 ===");
try {
  await invoke("cleanup_template_dir", { targetDir: TEST_DIR });
  await invoke("delete_vhost", { id: `${TEST_HOST}_${TEST_PORT}` });
  // 删测试目录
  fs.rmSync(TEST_DIR, { recursive: true, force: true });
  console.log("✓ 测试环境已清干净");
} catch (e) {
  console.log(`? 清理收尾失败: ${e}`);
}

ws.close();

console.log("\n========== 总结 ==========");
const all = [pass1, pass2, pass3];
const passed = all.filter(Boolean).length;
console.log(`通过 ${passed}/${all.length}`);
console.log(`  1. init_site_template 对脏目录报"目录非空": ${pass1 ? "✓" : "✗"}`);
console.log(`  2. cleanup_template_dir 正确清空非白名单: ${pass2 ? "✓" : "✗"}`);
console.log(`  3. cleanup 后 init_site_template 不再报"目录非空": ${pass3 ? "✓" : "✗"}`);
process.exit(passed === all.length ? 0 : 1);
