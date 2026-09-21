// @vitest-environment happy-dom
// 拖放动作选择弹层（rclone-ui parity）组件级冒烟：默认复制、方向键切换选项
// 并同步焦点、Enter 确认当前选择、底部按钮确认/取消、Esc 取消、关闭态不渲染。
import { afterEach, describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import DropActionDialog from "./DropActionDialog.vue";

const t = (key: string, values?: Record<string, string | number>) => key;

function mountDialog(open = true) {
  return mount(DropActionDialog, {
    attachTo: document.body,
    props: { open, targetPath: "/target", count: 2, t },
  });
}

async function settle(wrapper: VueWrapper) {
  await wrapper.vm.$nextTick();
  await nextTick();
}

function options(wrapper: VueWrapper) {
  return wrapper.findAll(".wb-dropaction-option");
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("DropActionDialog", () => {
  it("renders the radiogroup defaulting to copy with target/count meta", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    const group = wrapper.find('[role="radiogroup"]');
    expect(group.exists()).toBe(true);
    const rows = options(wrapper);
    expect(rows).toHaveLength(2);
    expect(rows[0].attributes("aria-checked")).toBe("true");
    expect(rows[0].classes()).toContain("is-active");
    expect(rows[1].attributes("aria-checked")).toBe("false");
    expect(wrapper.text()).toContain("dropActionItems");
    expect(wrapper.text()).toContain("dropActionTarget");
  });

  it("renders nothing while closed", () => {
    const wrapper = mountDialog(false);
    expect(wrapper.find(".wb-dialog").exists()).toBe(false);
  });

  it("focuses the first option on open and moves selection+focus with arrow keys", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    expect(document.activeElement).toBe(options(wrapper)[0].element);
    await options(wrapper)[0].trigger("keydown", { key: "ArrowDown" });
    expect(options(wrapper)[1].classes()).toContain("is-active");
    expect(document.activeElement).toBe(options(wrapper)[1].element);
    await options(wrapper)[1].trigger("keydown", { key: "ArrowUp" });
    expect(options(wrapper)[0].classes()).toContain("is-active");
  });

  it("confirms the selected action with Enter on an option", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    await options(wrapper)[0].trigger("keydown", { key: "ArrowDown" });
    await options(wrapper)[1].trigger("keydown", { key: "Enter" });
    expect(wrapper.emitted("choose")?.length).toBe(1);
    expect(wrapper.emitted("choose")![0][0]).toBe("move");
  });

  it("confirms via the footer confirm button with the current selection", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    await wrapper.get(".wb-dialog-primary").trigger("click");
    expect(wrapper.emitted("choose")?.length).toBe(1);
    expect(wrapper.emitted("choose")![0][0]).toBe("copy");
    await options(wrapper)[1].trigger("click");
    await wrapper.get(".wb-dialog-primary").trigger("click");
    expect(wrapper.emitted("choose")![1][0]).toBe("move");
  });

  it("emits cancel on Escape, backdrop click and the cancel button", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    await options(wrapper)[0].trigger("keydown", { key: "Escape" });
    expect(wrapper.emitted("cancel")?.length).toBe(1);
    await wrapper.get(".wb-dialog-cancel").trigger("click");
    expect(wrapper.emitted("cancel")?.length).toBe(2);
    await wrapper.get(".wb-dialog-backdrop").trigger("click");
    expect(wrapper.emitted("cancel")?.length).toBe(3);
    expect(wrapper.emitted("choose")).toBeUndefined();
  });

  it("traps Tab focus inside the dialog", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    // 末位（footer 确认钮）上 Tab 回绕到首位（copy 选项）。
    const confirm = wrapper.get(".wb-dialog-primary");
    (confirm.element as HTMLElement).focus();
    await confirm.trigger("keydown", { key: "Tab", shiftKey: false });
    expect(document.activeElement).toBe(options(wrapper)[0].element);
  });
});
