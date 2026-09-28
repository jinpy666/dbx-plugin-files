// @vitest-environment happy-dom
// ConnectionSelect（双栏连接选择下拉）：自定义弹层交互契约——打开/勾选/
// 选择即发 change/键盘导航与 Esc 关闭/点击外部关闭。切换语义（含失败回退）
// 在 App.paneConnectionRevert.spec 覆盖。
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import ConnectionSelect from "./ConnectionSelect.vue";

const options = [
  { id: "__local__", name: "Local files" },
  { id: "", name: "Same connection" },
  { id: "cloud-a", name: "Cloud A" },
];

const t = (key: string) => key;

function mountSelect(modelValue = "") {
  return mount(ConnectionSelect, {
    props: { modelValue, options, label: "Target connection", t },
    attachTo: document.body,
  });
}

describe("ConnectionSelect", () => {
  it("shows the current option label on the closed trigger", () => {
    const wrapper = mountSelect("cloud-a");
    const trigger = wrapper.find(".wb-conn-select-trigger");
    expect(trigger.attributes("title")).toBe("Cloud A");
    expect(trigger.attributes("aria-expanded")).toBe("false");
    expect(wrapper.find(".wb-conn-select-menu").exists()).toBe(false);
  });

  it("opens the menu with the active option marked and emits change on pick", async () => {
    const wrapper = mountSelect("");
    await wrapper.find(".wb-conn-select-trigger").trigger("click");
    const items = wrapper.findAll(".wb-conn-select-menu [role='option']");
    expect(items).toHaveLength(3);
    expect(items[1].attributes("aria-selected")).toBe("true");
    expect(items[1].classes()).toContain("is-active");

    await items[2].trigger("click");
    expect(wrapper.emitted("change")).toEqual([["cloud-a"]]);
    // 选择后收起。
    expect(wrapper.find(".wb-conn-select-menu").exists()).toBe(false);
  });

  it("closes on Escape and on outside click without emitting", async () => {
    const wrapper = mountSelect("");
    await wrapper.find(".wb-conn-select-trigger").trigger("click");
    await wrapper.find(".wb-conn-select-menu [role='option']").trigger("keydown", { key: "Escape" });
    expect(wrapper.find(".wb-conn-select-menu").exists()).toBe(false);
    expect(wrapper.emitted("change")).toBeUndefined();

    await wrapper.find(".wb-conn-select-trigger").trigger("click");
    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(".wb-conn-select-menu").exists()).toBe(false);
    expect(wrapper.emitted("change")).toBeUndefined();
  });

  it("supports arrow-key navigation into the menu (trigger ↓ opens)", async () => {
    const wrapper = mountSelect("");
    await wrapper.find(".wb-conn-select-trigger").trigger("keydown", { key: "ArrowDown", preventDefault: () => {} });
    expect(wrapper.find(".wb-conn-select-menu").exists()).toBe(true);
    expect(document.activeElement?.getAttribute("role")).toBe("option");
  });
});
