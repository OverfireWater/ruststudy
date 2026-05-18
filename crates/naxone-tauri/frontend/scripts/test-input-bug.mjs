// 通过 CDP 连 NaxOne webview，自动验证"输入框打不了字"是否复现。
// 前提：NaxOne 启动时带 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222
import puppeteer from "puppeteer-core";

// 直接通过 CDP /json 拿 NaxOne webview 的 webSocketDebuggerUrl，跳过 puppeteer 的 browser context 过滤
const resp = await fetch("http://127.0.0.1:9222/json");
const targets = await resp.json();
console.log(`所有 target (${targets.length}):`);
for (const t of targets) {
  console.log(`  - type=${t.type} title='${t.title}' url='${t.url}'`);
}
const naxoneTarget = targets.find(t => t.type === "page" && t.title === "NaxOne");
if (!naxoneTarget) {
  console.log("✗ 没找到 NaxOne webview target");
  process.exit(1);
}
console.log(`✓ 锁定 NaxOne target: id=${naxoneTarget.id}`);

const browser = await puppeteer.connect({
  browserWSEndpoint: naxoneTarget.webSocketDebuggerUrl,
  defaultViewport: null,
});

const pages = await browser.pages();
console.log(`pages after connect: ${pages.length}`);
const naxone = pages[0];
if (!naxone) {
  console.log("✗ connect 后没拿到 page");
  process.exit(1);
}
console.log(`✓ 锁定页面: url='${naxone.url()}'`);
console.log(`✓ 锁定页面: title='${await naxone.title()}' url='${naxone.url()}'`);

// 等首屏 vue 渲染
await naxone.waitForSelector("main", { timeout: 10000 }).catch(() => {});

// 列 main 内所有 input
const inputs = await naxone.evaluate(() => {
  const main = document.querySelector("main");
  if (!main) return { error: "no main" };
  const list = Array.from(main.querySelectorAll("input, textarea")).map((el) => ({
    tag: el.tagName,
    type: el.type || "",
    placeholder: el.placeholder || "",
    value: el.value || "",
    disabled: el.disabled,
    readOnly: el.readOnly,
  }));
  return { count: list.length, items: list };
});
console.log(`\n[diag] main 内输入元素: ${JSON.stringify(inputs, null, 2)}`);

// 找一个搜索框试输入
const searchSel = 'input[placeholder*="搜索"]';
const found = await naxone.$(searchSel);
if (!found) {
  console.log(`✗ 没找到 ${searchSel}，先切到"网站"页`);
  // 点侧栏"网站"
  await naxone.evaluate(() => {
    const items = Array.from(document.querySelectorAll("aside nav > div"));
    const w = items.find((el) => el.textContent.includes("网站"));
    w && w.click();
  });
  await new Promise(r => setTimeout(r, 1000));
}

const search = await naxone.$(searchSel);
if (!search) {
  console.log("✗ 切到网站后仍找不到搜索框");
  await browser.disconnect();
  process.exit(2);
}

console.log("\n=== 测试 1: click + type ===");
await search.click();
await new Promise(r => setTimeout(r, 200));
const active1 = await naxone.evaluate(() => {
  const a = document.activeElement;
  return a ? `${a.tagName}.${a.className?.split(" ").slice(0,2).join(".")}` : "null";
});
console.log(`click 后 activeElement: ${active1}`);

await search.type("test-abc-123", { delay: 30 });
const value1 = await search.evaluate(el => el.value);
console.log(`type 后 input.value: '${value1}'`);

console.log("\n=== 测试 2: 清空 + dispatch input event ===");
await search.evaluate(el => el.value = "");
await search.evaluate(el => {
  el.value = "synthetic-99";
  el.dispatchEvent(new Event("input", { bubbles: true }));
});
const value2 = await search.evaluate(el => el.value);
console.log(`dispatch input 后: '${value2}'`);

console.log("\n=== 测试 3: 检查全局 keydown 监听器是否吞 ===");
const blockers = await naxone.evaluate(() => {
  // 收集 window 上的事件监听器（getEventListeners 只在 devtools 可用）
  // 改用：发个 keydown 看是否被 preventDefault
  const inp = document.querySelector('main input[placeholder*="搜索"]');
  if (!inp) return "no input";
  inp.focus();
  const ev = new KeyboardEvent("keydown", { key: "a", bubbles: true, cancelable: true });
  const notCancelled = inp.dispatchEvent(ev);
  return { defaultPrevented: ev.defaultPrevented, dispatchedOk: notCancelled };
});
console.log(`合成 keydown 是否被吞: ${JSON.stringify(blockers)}`);

console.log("\n=== 测试 4: 检查覆盖在 input 上面的元素 ===");
const blockerEl = await naxone.evaluate(() => {
  const inp = document.querySelector('main input[placeholder*="搜索"]');
  if (!inp) return "no input";
  const rect = inp.getBoundingClientRect();
  const cx = rect.left + rect.width/2;
  const cy = rect.top + rect.height/2;
  const stack = document.elementsFromPoint(cx, cy);
  return stack.slice(0, 5).map(el => `${el.tagName}.${(el.className||"").toString().split(" ").slice(0,2).join(".")}`);
});
console.log(`input 中心点元素堆栈: ${JSON.stringify(blockerEl)}`);

await browser.disconnect();
console.log("\n--- 总结 ---");
console.log(value1 === "test-abc-123" ? "✓ click+type 能输入" : "✗ click+type 失败");
console.log(value2 === "synthetic-99" ? "✓ dispatch input 能改 v-model" : "✗ dispatch input 失败");
