// 自动化测试 cleanup_template_dir command + retryTemplate 集成链路。
//
// 验证场景：
//   1. 准备：找一个 vhost.document_root，往里塞模拟的 composer 残留（vendor/ + composer.json）
//   2. 调 cleanup_template_dir → 验证返回数量 + 文件被清 + htaccess 保留
//   3. 安全验证：传任意非 vhost 目录 → 必须报错
import WebSocket from "ws";
import fs from "node:fs";
import path from "node:path";

// 拿 NaxOne webview wsUrl
const targets = await (await fetch("http://127.0.0.1:9222/json")).json();
const t = targets.find((x) => x.type === "page" && x.url.startsWith("http://localhost:5173"));
if (!t) {
  console.log("✗ 未找到 NaxOne dev webview");
  process.exit(1);
}
console.log(`✓ 锁定 NaxOne: ${t.webSocketDebuggerUrl}`);

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
console.log("✓ WS 已连");

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
    }, 10000);
  });
}

async function invoke(cmd, args = {}) {
  // 调用 Tauri IPC。Tauri 2 通过 window.__TAURI_INTERNALS__.invoke。
  const argsJson = JSON.stringify(args).replace(/'/g, "\\'");
  const expr = `
    (async () => {
      try {
        const inv = window.__TAURI_INTERNALS__?.invoke
          || window.__TAURI__?.core?.invoke
          || window.__TAURI__?.invoke;
        if (!inv) return { __err: "no invoke" };
        const r = await inv("${cmd}", ${argsJson});
        return { __ok: r };
      } catch (e) {
        return { __err: String(e) };
      }
    })()
  `;
  const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error("JS exception: " + JSON.stringify(r.exceptionDetails));
  const v = r.result.value;
  if (v.__err) throw new Error(v.__err);
  return v.__ok;
}

// 等首屏 vue 渲染好
await send("Runtime.evaluate", {
  expression: `new Promise(r => { const t = () => document.querySelector("main") ? r(true) : setTimeout(t, 100); t(); })`,
  awaitPromise: true,
});
console.log("✓ Vue 渲染就绪");

// ============ Step 1: 准备测试环境 ============
console.log("\n=== Step 1: 拿 vhost 列表 ===");
const vhosts = await invoke("get_vhosts");
console.log(`vhost 数: ${vhosts.length}`);
if (vhosts.length === 0) {
  console.log("✗ 没有 vhost 可测，请先建一个");
  ws.close();
  process.exit(1);
}
const v = vhosts[0];
console.log(`使用 vhost: ${v.server_name} doc_root=${v.document_root}`);

const docRoot = v.document_root.replace(/\//g, path.sep);

// ============ Step 2: 塞测试残留 ============
console.log("\n=== Step 2: 塞模拟 composer 残留 ===");
// 备份现有内容
const beforeFiles = fs.readdirSync(docRoot);
console.log(`之前文件: ${JSON.stringify(beforeFiles)}`);

// 写几个模拟 composer 残留
fs.writeFileSync(path.join(docRoot, "composer.json"), '{"name":"test/dummy"}');
fs.writeFileSync(path.join(docRoot, "composer.lock"), '{}');
fs.mkdirSync(path.join(docRoot, "vendor"), { recursive: true });
fs.writeFileSync(path.join(docRoot, "vendor", "autoload.php"), "<?php\n");
fs.mkdirSync(path.join(docRoot, "vendor", "fake-pkg"), { recursive: true });
fs.writeFileSync(path.join(docRoot, "vendor", "fake-pkg", "Foo.php"), "<?php\n");
// 同时确保 nginx.htaccess / .htaccess 存在（如果不存在则创建，模拟正常 vhost 状态）
const ha = path.join(docRoot, "nginx.htaccess");
if (!fs.existsSync(ha)) fs.writeFileSync(ha, "location / { try_files $uri /index.php?$query_string; }");
const dotHa = path.join(docRoot, ".htaccess");
if (!fs.existsSync(dotHa)) fs.writeFileSync(dotHa, "RewriteEngine On");

const afterPrep = fs.readdirSync(docRoot);
console.log(`塞入后: ${JSON.stringify(afterPrep)}`);

// ============ Step 3: 调 cleanup_template_dir ============
console.log("\n=== Step 3: 调 cleanup_template_dir ===");
// 转回前端格式（正斜杠）
const targetDir = v.document_root;
console.log(`targetDir: ${targetDir}`);
const cleaned = await invoke("cleanup_template_dir", { targetDir });
console.log(`✓ cleanup 返回: ${cleaned} 项`);

const afterClean = fs.readdirSync(docRoot);
console.log(`清完剩余: ${JSON.stringify(afterClean)}`);

// 断言
const survived = afterClean.filter((f) => !["nginx.htaccess", ".htaccess"].includes(f));
console.log(`\n非白名单残留: ${JSON.stringify(survived)}`);

let pass1 = survived.length === 0;
let pass2 = afterClean.includes("nginx.htaccess");
let pass3 = afterClean.includes(".htaccess");

console.log(`\n✓ 测试 3.1 ${pass1 ? "通过" : "✗ 失败"}：非白名单文件已全清`);
console.log(`✓ 测试 3.2 ${pass2 ? "通过" : "✗ 失败"}：nginx.htaccess 保留`);
console.log(`✓ 测试 3.3 ${pass3 ? "通过" : "✗ 失败"}：.htaccess 保留`);

// ============ Step 4: 安全门禁验证 ============
console.log("\n=== Step 4: 安全门禁验证 ===");
let pass4 = false;
try {
  const r = await invoke("cleanup_template_dir", { targetDir: "C:\\Windows" });
  console.log(`✗ 失败：cleanup 居然清了 C:\\Windows，返回 ${r}`);
} catch (e) {
  if (String(e).includes("拒绝清理") || String(e).includes("不是任何 vhost")) {
    console.log(`✓ 测试 4 通过：传 C:\\Windows 被拒绝 (${e})`);
    pass4 = true;
  } else {
    console.log(`✗ 测试 4 失败：报错了但不是预期门禁错误：${e}`);
  }
}

// 非绝对路径
let pass5 = false;
try {
  await invoke("cleanup_template_dir", { targetDir: "relative/path" });
  console.log("✗ 失败：相对路径居然被接受");
} catch (e) {
  if (String(e).includes("绝对路径") || String(e).includes("拒绝清理")) {
    console.log(`✓ 测试 5 通过：相对路径被拒绝 (${e})`);
    pass5 = true;
  }
}

ws.close();

console.log("\n=========== 总结 ===========");
const all = [pass1, pass2, pass3, pass4, pass5];
const passed = all.filter(Boolean).length;
console.log(`通过 ${passed}/${all.length}`);
process.exit(passed === all.length ? 0 : 1);
