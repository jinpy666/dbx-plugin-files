// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import ConfirmDialog from "./ConfirmDialog.vue";

const baseProps = {
  open: false,
  title: "确认",
  confirmLabel: "确认",
  cancelLabel: "取消",
};

async function flush(times = 3) {
  for (let i = 0; i < times; i += 1) await Promise.resolve();
}

describe("ConfirmDialog (P1-3 焦点管理)", () => {
  it("focuses the form input on open when slot input exists", async () => {
    const wrapper = mount(ConfirmDialog, {
      props: { ...baseProps, open: true },
      slots: { default: '<input data-test="name" />' },
      attachTo: document.body,
    });
    await flush();
    expect(document.activeElement).toBe(wrapper.find("input").element);
    wrapper.unmount();
  });

  it("focuses the safe cancel button on open when no input exists", async () => {
    const wrapper = mount(ConfirmDialog, {
      props: { ...baseProps, open: true, danger: true },
      attachTo: document.body,
    });
    await flush();
    expect(document.activeElement).toBe(wrapper.find(".wb-dialog-cancel").element);
    wrapper.unmount();
  });

  it("traps Tab focus inside the dialog and wraps at both ends", async () => {
    const wrapper = mount(ConfirmDialog, {
      props: { ...baseProps, open: true },
      attachTo: document.body,
    });
    await flush();
    const dialog = wrapper.find(".wb-dialog");
    const cancel = wrapper.find(".wb-dialog-cancel").element as HTMLButtonElement;
    const confirm = wrapper.find(".wb-dialog-primary").element as HTMLButtonElement;
    // 取消（首位）→ Shift+Tab 回绕到确认（末位）
    cancel.focus();
    await dialog.trigger("keydown", { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(confirm);
    // 确认（末位）→ Tab 回绕到取消（首位）
    await dialog.trigger("keydown", { key: "Tab" });
    expect(document.activeElement).toBe(cancel);
    wrapper.unmount();
  });

  it("returns focus to the triggering element after close", async () => {
    const wrapper = mount(
      {
        components: { ConfirmDialog },
        data: () => ({ open: false }),
        template: `
          <div>
            <button data-test="trigger">触发</button>
            <ConfirmDialog :open="open" title="t" confirm-label="ok" cancel-label="no" />
          </div>
        `,
        mounted() {
          (this as unknown as { $el: HTMLElement }).$el.querySelector("button")?.focus();
        },
      },
      { attachTo: document.body },
    );
    const trigger = wrapper.find("[data-test=trigger]").element;
    expect(document.activeElement).toBe(trigger);
    await wrapper.setData({ open: true });
    await flush();
    expect(document.activeElement).not.toBe(trigger);
    await wrapper.setData({ open: false });
    await flush();
    expect(document.activeElement).toBe(trigger);
    wrapper.unmount();
  });

  it("does not steal focus when closed in the same tick as open", async () => {
    const wrapper = mount(ConfirmDialog, {
      props: { ...baseProps, open: false },
      attachTo: document.body,
    });
    await wrapper.setProps({ open: true });
    // 立即关闭（同一宏任务内）：nextTick 后发现已关闭，不抢焦点也不归还错乱
    await wrapper.setProps({ open: false });
    await flush();
    expect(wrapper.find(".wb-dialog").exists()).toBe(false);
    wrapper.unmount();
  });
});
