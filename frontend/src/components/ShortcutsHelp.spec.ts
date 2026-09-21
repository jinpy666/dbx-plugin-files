// @vitest-environment happy-dom
// 快捷键速查弹层单测：三组快捷键渲染、Esc/关闭钮/遮罩点击关闭、焦点入容器。
import { afterEach, describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import ShortcutsHelp from "./ShortcutsHelp.vue";

let wrapper: VueWrapper | undefined;

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
});

const t = (key: string) => key;

describe("ShortcutsHelp", () => {
  it("lists browse/selection/action groups with their keys", () => {
    wrapper = mount(ShortcutsHelp, { props: { t }, attachTo: document.body });
    const body = wrapper.get("[data-test=shortcuts-body]");
    for (const group of ["shortcutsGroupBrowse", "shortcutsGroupSelect", "shortcutsGroupActions"]) {
      expect(body.text()).toContain(group);
    }
    for (const label of ["scArrowNav", "scShiftArrow", "scEnterOpen", "scSelectAll", "scF2Rename", "scDeleteKey"]) {
      expect(body.text()).toContain(label);
    }
  });

  it("moves focus into the overlay on open and closes on Escape", async () => {
    wrapper = mount(ShortcutsHelp, { props: { t }, attachTo: document.body });
    // onMounted 经 nextTick 聚焦，等几个微任务轮再断言。
    for (let i = 0; i < 6; i += 1) await Promise.resolve();
    expect(document.activeElement).toBe(wrapper.find(".wb-dialog-backdrop").element);
    await wrapper.find(".wb-dialog-backdrop").trigger("keydown", { key: "Escape" });
    expect(wrapper.emitted("close")).toHaveLength(1);
  });

  it("closes via backdrop click and the close button", async () => {
    wrapper = mount(ShortcutsHelp, { props: { t }, attachTo: document.body });
    await wrapper.find(".wb-dialog-backdrop").trigger("click");
    expect(wrapper.emitted("close")).toHaveLength(1);
    await wrapper.get("[data-test=shortcuts-close]").trigger("click");
    expect(wrapper.emitted("close")).toHaveLength(2);
  });
});
