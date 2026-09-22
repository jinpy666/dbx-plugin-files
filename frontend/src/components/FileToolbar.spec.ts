// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mount, config } from "@vue/test-utils";
import FileToolbar from "./FileToolbar.vue";
import { vTip } from "../lib/tooltip";

// 模板里的 v-tip（图标按钮提示）在测试挂载时同样需要指令注册。
config.global.directives = { tip: vTip };

const t = (key: string, values?: Record<string, string | number>) =>
  values && "rate" in values ? `${key}:${values.rate}` : key;

type ToolbarProps = InstanceType<typeof FileToolbar>["$props"];

function mountToolbar(overrides: Partial<ToolbarProps> = {}) {
  return mount(FileToolbar, {
    attachTo: document.body,
    props: {
      canWrite: true,
      busy: false,
      hasSelection: false,
      dockOpen: false,
      dockTab: "transfers",
      dualPane: false,
      connectionName: "OSS",
      readOnly: false,
      connState: "connected",
      showMount: false,
      canMount: false,
      bwlimit: null,
      starred: false,
      t,
      ...overrides,
    },
  });
}

function bwlimitButton(wrapper: ReturnType<typeof mountToolbar>) {
  return wrapper.find(".wb-bwlimit-toggle");
}

async function openMenu(wrapper: ReturnType<typeof mountToolbar>) {
  await bwlimitButton(wrapper).trigger("click");
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  document.body.innerHTML = "";
});

describe("FileToolbar bwlimit quick control (令牌桶常驻切换入口)", () => {
  it("shows the bwlimit button even when no limit is set (旧徽标只在生效时可见)", () => {
    const wrapper = mountToolbar();
    expect(bwlimitButton(wrapper).exists()).toBe(true);
    expect(bwlimitButton(wrapper).classes()).not.toContain("is-active");
    expect(wrapper.find(".wb-bwlimit-rate").exists()).toBe(false);
    wrapper.unmount();
  });

  it("marks the control active and inlines the rate when a limit applies", () => {
    const wrapper = mountToolbar({ bwlimit: "10M" });
    expect(bwlimitButton(wrapper).classes()).toContain("is-active");
    expect(wrapper.find(".wb-bwlimit-rate").text()).toBe("10M");
    wrapper.unmount();
  });

  it("opens a menu with presets and applies one via bwlimit-set", async () => {
    const wrapper = mountToolbar();
    await openMenu(wrapper);
    const items = wrapper.findAll(".wb-bwlimit-menu button");
    expect(items.length).toBe(6); // 4 presets + 不限速 + 自定义
    await items[0].trigger("click");
    expect(wrapper.emitted("bwlimit-set")?.length).toBe(1);
    expect(wrapper.emitted("bwlimit-set")![0][0]).toBe("1M");
    // 选择后菜单收起。
    expect(wrapper.find(".wb-bwlimit-menu").exists()).toBe(false);
    wrapper.unmount();
  });

  it("emits an empty rate for the unlimited item (off)", async () => {
    const wrapper = mountToolbar({ bwlimit: "5M" });
    await openMenu(wrapper);
    const items = wrapper.findAll(".wb-bwlimit-menu button");
    await items[4].trigger("click");
    expect(wrapper.emitted("bwlimit-set")![0][0]).toBe("");
    wrapper.unmount();
  });

  it("routes the custom item to the settings dialog via bwlimit-click", async () => {
    const wrapper = mountToolbar();
    await openMenu(wrapper);
    const items = wrapper.findAll(".wb-bwlimit-menu button");
    await items[5].trigger("click");
    expect(wrapper.emitted("bwlimit-click")?.length).toBe(1);
    expect(wrapper.emitted("bwlimit-set")).toBeUndefined();
    wrapper.unmount();
  });

  it("closes the menu when clicking outside (capture pointerdown)", async () => {
    const wrapper = mountToolbar();
    await openMenu(wrapper);
    expect(wrapper.find(".wb-bwlimit-menu").exists()).toBe(true);
    document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(".wb-bwlimit-menu").exists()).toBe(false);
    wrapper.unmount();
  });
});

describe("FileToolbar stats dock entry", () => {
  it("offers a dedicated stats icon that opens the dock on the stats tab", async () => {
    const wrapper = mountToolbar();
    await wrapper.find(".wb-stats-toggle").trigger("click");
    const dockEvents = wrapper.emitted("toggle-dock");
    expect(dockEvents?.length).toBe(1);
    expect(dockEvents![0][0]).toBe("stats");
    wrapper.unmount();
  });

  it("hides the dedicated stats entry while the dock is open on stats (dock 开关已承载)", () => {
    const wrapper = mountToolbar({ dockOpen: true, dockTab: "stats" });
    expect(wrapper.find(".wb-stats-toggle").exists()).toBe(false);
    // 唯一高亮按钮即 dock 开关（此时显示 stats 页签的 Activity 图标）。
    const activeButtons = wrapper.findAll(".wb-toolbar-actions button.is-active");
    expect(activeButtons.length).toBe(1);
    wrapper.unmount();
  });
});
