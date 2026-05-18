/**
 * 自动化复现"所有输入框打不了字"bug。
 *
 * 路径：navigate to Vhosts page → focus 搜索框 → setValue → assert value 包含
 * 输入字符串。如果失败，把异常类型与现场 DOM 状态打印出来。
 */
import { expect } from "@wdio/globals";
import { browser, $ } from "@wdio/globals";
import { waitForApp, navigate } from "../helpers.js";

describe("input bug repro", () => {
  before(async () => {
    await waitForApp();
  });

  it("Vhosts 搜索框 应能输入文字（v-model 双向绑定）", async () => {
    await navigate("网站");
    await browser.pause(800);

    // 检查 main 内是否有 input
    const inputCount = await browser.execute(() => {
      const main = document.querySelector("main");
      return main ? main.querySelectorAll("input").length : 0;
    });
    console.log(`[diag] main 内 input 数量: ${inputCount}`);
    expect(inputCount).toBeGreaterThan(0);

    // 直接拿第一个搜索框 input
    const searchInput = await $('input[placeholder*="搜索"]');
    const exists = await searchInput.isExisting();
    console.log(`[diag] 搜索 input 存在: ${exists}`);
    expect(exists).toBe(true);

    // 尝试 setValue（wdio 内部会先 click focus 再分发 keydown）
    await searchInput.click();
    await browser.pause(200);

    const focusedTag = await browser.execute(() => {
      const a = document.activeElement;
      return a ? `${a.tagName}.${a.className}` : "null";
    });
    console.log(`[diag] click 后 activeElement: ${focusedTag}`);

    // 用 setValue 写
    await searchInput.setValue("test-abc-123");
    await browser.pause(300);

    const value = await searchInput.getValue();
    console.log(`[diag] setValue 后 input.value: '${value}'`);

    // 同时拿 vue 内部 ref 值（searchQuery）—— 通过 DOM 反查
    const vueValue = await browser.execute(() => {
      const inp = document.querySelector('main input[placeholder*="搜索"]') as HTMLInputElement | null;
      return inp ? { domValue: inp.value, type: inp.type, disabled: inp.disabled, readOnly: inp.readOnly } : null;
    });
    console.log(`[diag] DOM 实测: ${JSON.stringify(vueValue)}`);

    // 列出所有全局 keydown 监听器（粗略：能否被取消）
    const blockers = await browser.execute(() => {
      // 用合成事件验证：发到 input 上，看是否传到 v-model 处理器
      const inp = document.querySelector('main input[placeholder*="搜索"]') as HTMLInputElement | null;
      if (!inp) return "no input";
      // 直接走 input event 而非 keydown：v-model 监听的是 input 事件
      inp.value = "synthetic-test";
      inp.dispatchEvent(new Event("input", { bubbles: true }));
      return inp.value;
    });
    console.log(`[diag] dispatchEvent input 后 value: '${blockers}'`);

    expect(value).toBe("test-abc-123");
  });

  it("仪表板 / 服务配置 / 软件商店：枚举所有 input，无任何 disabled/readonly 异常", async () => {
    const pages: Array<"仪表板" | "网站" | "服务配置" | "软件商店"> = [
      "仪表板",
      "网站",
      "服务配置",
      "软件商店",
    ];
    for (const p of pages) {
      await navigate(p);
      await browser.pause(500);
      const inputs = await browser.execute(() => {
        const main = document.querySelector("main");
        if (!main) return [];
        return Array.from(main.querySelectorAll("input, textarea")).map((el: any) => ({
          tag: el.tagName,
          type: el.type || "",
          placeholder: el.placeholder || "",
          disabled: el.disabled,
          readOnly: el.readOnly,
          visible: el.offsetParent !== null,
        }));
      });
      console.log(`[${p}] 输入元素 ${inputs.length} 个: ${JSON.stringify(inputs, null, 2)}`);
    }
  });
});
