// CSP 验证 GUI 自动化：
//
//   1. 通过 CDP 连 NaxOne dev（端口 9222）
//   2. 订阅 Console / Runtime / Log 三个 domain 的所有消息
//   3. 程序化点击各个主 tab：仪表板 / 网站 / 软件商店 / 服务配置 / 设置
//      服务配置内再点 PHP → phpinfo（这是 CSP 最容易翻车的地方，iframe srcdoc）
//   4. 收集期间所有 Console 错误 + CSP violation
//   5. 报告是否有 "Content Security Policy" / "Refused to" 类违规
//
// 退出码：0 = 无 CSP 违规，1 = 有违规或脚本失败
import WebSocket from "ws";

const targets = await (await fetch("http://127.0.0.1:9222/json")).json();
const t = targets.find((x) => x.type === "page" && x.url.startsWith("http://localhost:5173"));
if (!t) {
  console.error("✗ 未找到 NaxOne webview（dev 没起？端口 9222 没开？）");
  process.exit(1);
}
console.log(`✓ 找到 webview: ${t.url}`);

const ws = new WebSocket(t.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
const consoleLogs = []; // {level, text, source}
const cspViolations = []; // CSP violation 文本
const networkFails = []; // 失败的 net 请求

ws.on("message", (data) => {
  const m = JSON.parse(data.toString());
  if (m.id && pending.has(m.id)) {
    const { resolve, reject } = pending.get(m.id);
    pending.delete(m.id);
    if (m.error) reject(new Error(m.error.message || JSON.stringify(m.error)));
    else resolve(m.result);
    return;
  }
  // 事件订阅
  if (m.method === "Log.entryAdded") {
    const e = m.params.entry;
    consoleLogs.push({ level: e.level, text: e.text, source: e.source });
    if (e.source === "security" || e.source === "violation" ||
        (e.text && e.text.toLowerCase().includes("content security policy"))) {
      cspViolations.push(e.text);
      console.log(`  ⚠ CSP violation: ${e.text.substring(0, 200)}`);
    }
  } else if (m.method === "Runtime.consoleAPICalled") {
    const args = (m.params.args || []).map(a => a.value ?? a.description ?? "").join(" ");
    consoleLogs.push({ level: m.params.type, text: args, source: "console" });
    if (args && args.toLowerCase().includes("content security policy")) {
      cspViolations.push(args);
      console.log(`  ⚠ CSP violation (console): ${args.substring(0, 200)}`);
    }
  } else if (m.method === "Network.loadingFailed") {
    const url = m.params.requestId; // 拿不到 url 这里只记 reqId 调试用
    if (m.params.blockedReason) {
      networkFails.push(`blocked: ${m.params.blockedReason} (reqId=${url})`);
      console.log(`  ⚠ Net blocked: ${m.params.blockedReason}`);
    }
  } else if (m.method === "Page.frameRequestedNavigation") {
    // 调试用
  }
});

await new Promise((r) => ws.once("open", r));

function send(method, params = {}) {
  const myId = ++id;
  return new Promise((resolve, reject) => {
    pending.set(myId, { resolve, reject });
    ws.send(JSON.stringify({ id: myId, method, params }));
    setTimeout(() => {
      if (pending.has(myId)) { pending.delete(myId); reject(new Error(`timeout: ${method}`)); }
    }, 15000);
  });
}

async function evalJs(expr) {
  const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) {
    consoleLogs.push({ level: "error", text: "JS eval: " + JSON.stringify(r.exceptionDetails), source: "test" });
    throw new Error("JS exception: " + JSON.stringify(r.exceptionDetails));
  }
  return r.result.value;
}

async function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

// 启用监听 domain
await send("Log.enable");
await send("Runtime.enable");
await send("Network.enable");
await send("Page.enable");

// 等 Vue 就绪
console.log("\n=== 等 Vue 就绪 ===");
await evalJs(`new Promise(r => { const f = () => document.querySelector("nav, aside, .sidebar, [class*='sidebar']") ? r(true) : setTimeout(f, 100); f(); })`);
console.log("✓ Vue 就绪");

// 主 tab 点击 —— 用 Vue Router 导航，比找 DOM 稳
const routes = [
  { path: "/dashboard", name: "仪表板" },
  { path: "/vhosts", name: "网站" },
  { path: "/store", name: "软件商店" },
  { path: "/service-config", name: "服务配置" },
  { path: "/settings", name: "设置" },
];

console.log("\n=== 主 tab 巡检 ===");
for (const r of routes) {
  console.log(`→ 导航 ${r.name}`);
  await evalJs(`
    (async () => {
      // Vue Router 走 hash 或 history 模式都试一遍
      if (window.__VUE_ROUTER__) {
        await window.__VUE_ROUTER__.push("${r.path}");
      } else {
        location.hash = "#${r.path}";
        // 备选：history mode
        if (!location.hash.includes("${r.path}")) {
          history.pushState({}, "", "${r.path}");
          window.dispatchEvent(new PopStateEvent("popstate"));
        }
      }
    })()
  `);
  await sleep(800);
}

// 服务配置内的 PHP / phpinfo 子 tab（CSP 最关键的地方）
console.log("\n=== 服务配置 → PHP → phpinfo ===");
await evalJs(`location.hash = "#/service-config"`);
await sleep(800);
// 点 PHP tab
const phpTabClicked = await evalJs(`
  (() => {
    const buttons = [...document.querySelectorAll('button, a, [role="tab"]')];
    const php = buttons.find(b => b.textContent.trim() === 'PHP');
    if (php) { php.click(); return true; }
    return false;
  })()
`);
console.log(phpTabClicked ? "✓ 点了 PHP tab" : "✗ 没找到 PHP tab 按钮");
await sleep(500);

// 点 phpinfo 子 tab
const phpinfoClicked = await evalJs(`
  (() => {
    const buttons = [...document.querySelectorAll('button, a')];
    const phpinfo = buttons.find(b => b.textContent.trim() === 'phpinfo');
    if (phpinfo) { phpinfo.click(); return true; }
    return false;
  })()
`);
console.log(phpinfoClicked ? "✓ 点了 phpinfo 子 tab" : "✗ 没找到 phpinfo 子 tab");
await sleep(3000); // 等 phpinfo 加载 + iframe srcdoc 渲染

// 检查 iframe 是否真的渲染了内容
const iframeInfo = await evalJs(`
  (() => {
    const iframe = document.querySelector('iframe');
    if (!iframe) return { exists: false };
    try {
      const doc = iframe.contentDocument || iframe.contentWindow?.document;
      const hasContent = doc && doc.body && doc.body.innerHTML.length > 100;
      const title = doc?.title || "";
      const hasPhpinfoTable = doc?.querySelector("h1, h2") ? true : false;
      return {
        exists: true,
        hasContent,
        title,
        hasPhpinfoTable,
        bodyLen: doc?.body?.innerHTML?.length || 0,
      };
    } catch (e) {
      return { exists: true, error: String(e) };
    }
  })()
`);
console.log("iframe 状态:", JSON.stringify(iframeInfo));

// 关闭
await sleep(500);

// === 报告 ===
console.log("\n========================================");
console.log("=== 测试报告 ===");
console.log("========================================");

const errLogs = consoleLogs.filter(l => l.level === "error" || l.level === "warning");
console.log(`总 console 消息：${consoleLogs.length}（其中 error/warn ${errLogs.length}）`);
console.log(`CSP violations：${cspViolations.length}`);
console.log(`Network blocked：${networkFails.length}`);

if (errLogs.length > 0) {
  console.log("\n--- error/warning 消息（去重前 20 条）---");
  const seen = new Set();
  for (const l of errLogs) {
    const key = (l.text || "").substring(0, 100);
    if (seen.has(key)) continue;
    seen.add(key);
    if (seen.size > 20) break;
    console.log(`  [${l.level}] ${l.source}: ${(l.text || "").substring(0, 200)}`);
  }
}

if (cspViolations.length > 0) {
  console.log("\n--- CSP 违规明细 ---");
  for (const v of cspViolations) console.log(`  • ${v.substring(0, 300)}`);
  console.log("\n✗ CSP 配置过严，需要放宽");
  process.exit(1);
}

if (iframeInfo.exists && !iframeInfo.hasContent && iframeInfo.error) {
  console.log(`\n⚠ phpinfo iframe 异常：${iframeInfo.error}`);
}
if (iframeInfo.exists && iframeInfo.bodyLen === 0) {
  console.log("\n⚠ phpinfo iframe 内容为空（可能没装 PHP 或刚切到 tab 还没加载）");
}

if (cspViolations.length === 0 && errLogs.length < 5) {
  console.log("\n✓ CSP 通过：无违规");
  process.exit(0);
} else {
  console.log("\n? 有错误但非 CSP，请人工检查日志");
  process.exit(0);
}
