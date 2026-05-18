// 直接 CDP WebSocket，避开 puppeteer Browser domain（WebView2 不支持）。
// 只用 Runtime.evaluate 跑 JS 注入测试。
import WebSocket from "ws";

// 拿 NaxOne webview 的 wsUrl
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
    }, 8000);
  });
}

async function evaluate(expr) {
  const r = await send("Runtime.evaluate", {
    expression: expr,
    returnByValue: true,
    awaitPromise: true,
  });
  if (r.exceptionDetails) {
    throw new Error("JS exception: " + (r.exceptionDetails.text || JSON.stringify(r.exceptionDetails)));
  }
  return r.result.value;
}

// 等首屏渲染
console.log("\n[1] 等 main 元素就绪");
await evaluate(`
new Promise(resolve => {
  const tick = () => document.querySelector("main") ? resolve(true) : setTimeout(tick, 100);
  tick();
})`);
console.log("✓ main 已渲染");

// 切到「网站」页
console.log("\n[2] 切到 网站 页");
await evaluate(`
(function(){
  const items = Array.from(document.querySelectorAll("aside nav > div"));
  const w = items.find(el => el.textContent.includes("网站"));
  if (!w) return "no nav item";
  w.click();
  return "clicked";
})()`);
await new Promise((r) => setTimeout(r, 1200));

// 列 main 内所有 input
console.log("\n[3] 列 main 内所有 input/textarea");
const inputs = await evaluate(`
(function(){
  const main = document.querySelector("main");
  if (!main) return { error: "no main" };
  return Array.from(main.querySelectorAll("input,textarea")).map(el => ({
    tag: el.tagName,
    type: el.type || "",
    placeholder: el.placeholder || "",
    value: el.value || "",
    disabled: el.disabled,
    readOnly: el.readOnly,
    visible: el.offsetParent !== null,
  }));
})()`);
console.log(JSON.stringify(inputs, null, 2));

// 测试输入（核心场景）
console.log("\n[4] 测试搜索框 v-model 输入");
const result = await evaluate(`
(function(){
  const inp = document.querySelector('main input[placeholder*="搜索"]');
  if (!inp) return { error: "no search input" };
  // 1. focus
  inp.focus();
  const focused = document.activeElement === inp;
  // 2. 模拟 keydown + 检查是否被吞
  const evK = new KeyboardEvent("keydown", { key: "a", bubbles: true, cancelable: true });
  inp.dispatchEvent(evK);
  const kdPrevented = evK.defaultPrevented;
  // 3. 直接走 input 事件（v-model 监听 input）
  inp.value = "auto-test-123";
  inp.dispatchEvent(new Event("input", { bubbles: true }));
  // 4. 读 DOM 实际 value
  return {
    focused,
    keydownDefaultPrevented: kdPrevented,
    valueAfterInput: inp.value,
    disabled: inp.disabled,
    readOnly: inp.readOnly,
    pointerEvents: getComputedStyle(inp).pointerEvents,
  };
})()`);
console.log(JSON.stringify(result, null, 2));

// 测试 CDP 物理键盘输入
console.log("\n[5] 测试 CDP Input.dispatchKeyEvent（模拟真键盘）");
// 先 focus
await evaluate(`document.querySelector('main input[placeholder*="搜索"]').focus()`);
// 清空
await evaluate(`document.querySelector('main input[placeholder*="搜索"]').value = ""`);
// 发 CDP 文本输入
await send("Input.insertText", { text: "physical-key-99" });
await new Promise((r) => setTimeout(r, 300));
const phys = await evaluate(`document.querySelector('main input[placeholder*="搜索"]').value`);
console.log(`CDP Input.insertText 后 value: '${phys}'`);

// 检查 input 中心点的元素堆栈
console.log("\n[6] input 中心点元素堆栈（找覆盖元素）");
const stack = await evaluate(`
(function(){
  const inp = document.querySelector('main input[placeholder*="搜索"]');
  if (!inp) return [];
  const r = inp.getBoundingClientRect();
  return document.elementsFromPoint(r.left + r.width/2, r.top + r.height/2).slice(0,5).map(el =>
    el.tagName + "." + (el.className ? el.className.toString().split(" ").slice(0,2).join(".") : ""));
})()`);
console.log(JSON.stringify(stack));

ws.close();
console.log("\n=== 结论 ===");
console.log(result.valueAfterInput === "auto-test-123" ? "✓ v-model 输入正常" : "✗ v-model 输入失败");
console.log(phys === "physical-key-99" ? "✓ 物理键盘输入正常" : "✗ 物理键盘输入失败（bug 复现）");
