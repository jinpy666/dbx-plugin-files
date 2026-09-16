// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { defineComponent } from "vue";
import { mount } from "@vue/test-utils";
import { vTip } from "./tooltip";

// v-tip 是图标按钮的悬浮提示实现：宿主 webview 不渲染原生 title，
// 这里锁定 aria-label 同步、延迟显示、移出/按下即隐藏的行为。

function mountTip(tip?: string) {
  return mount(
    defineComponent({
      props: { tip: { type: String, default: undefined } },
      template: `<button v-tip="tip" type="button">btn</button>`,
    }),
    { props: { tip }, global: { directives: { tip: vTip } } },
  );
}

const visibleTip = () => document.querySelector<HTMLElement>(".wb-global-tip.is-visible");

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("vTip directive", () => {
  it("syncs aria-label from the bound text", () => {
    const wrapper = mountTip("Refresh");
    expect(wrapper.attributes("aria-label")).toBe("Refresh");
  });

  it("drops aria-label when the bound text is empty", () => {
    const wrapper = mountTip("");
    expect(wrapper.attributes("aria-label")).toBeUndefined();
  });

  it("follows binding updates (locale switch)", async () => {
    const wrapper = mountTip("Refresh");
    await wrapper.setProps({ tip: "Refresh " });
    expect(wrapper.attributes("aria-label")).toBe("Refresh");
  });

  it("shows the tooltip after the hover delay and hides on leave", async () => {
    const wrapper = mountTip("Dual pane");
    await wrapper.trigger("mouseenter");
    vi.advanceTimersByTime(100);
    expect(visibleTip()).toBeNull();
    vi.advanceTimersByTime(300);
    expect(visibleTip()?.textContent).toBe("Dual pane");
    await wrapper.trigger("mouseleave");
    expect(visibleTip()).toBeNull();
  });

  it("hides as soon as the button is pressed", async () => {
    const wrapper = mountTip("Retry");
    await wrapper.trigger("mouseenter");
    vi.advanceTimersByTime(400);
    expect(visibleTip()).not.toBeNull();
    await wrapper.trigger("mousedown");
    expect(visibleTip()).toBeNull();
  });

  it("does not show anything without bound text", async () => {
    const wrapper = mountTip("");
    await wrapper.trigger("mouseenter");
    vi.advanceTimersByTime(500);
    expect(visibleTip()).toBeNull();
  });
});
