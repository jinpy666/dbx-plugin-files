// @vitest-environment happy-dom
// SyncDialog 组件单测：kind 决定标题与提示、确认载荷组装（模式拆分、整数
// 夹紧、空字段不下发）、目标路径为空时确认禁用、取消/关闭事件。
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import SyncDialog from "./SyncDialog.vue";
import { workbenchMessage } from "../lib/i18n";

const t = (key: string, values?: Record<string, string | number>) =>
  workbenchMessage("en", key, values);

function dialog() {
  return mount(SyncDialog, {
    props: {
      t,
      kind: "syncDir" as const,
      sourcePath: "/reports/2024",
      defaultTarget: "/mirror/2024",
    },
  });
}

describe("SyncDialog", () => {
  it("shows the sync title, mirror warning and the fixed source path", () => {
    const wrapper = dialog();
    expect(wrapper.find("header").text()).toContain(workbenchMessage("en", "transferKind.syncDir"));
    expect(wrapper.find("output").text()).toBe("/reports/2024");
    // sync 镜像警示（copyDir 无删除语义，不显示）。
    expect(wrapper.text()).toContain(workbenchMessage("en", "syncDirBody"));
    wrapper.unmount();

    const copy = mount(SyncDialog, {
      props: { t, kind: "copyDir" as const, sourcePath: "/a", defaultTarget: "/b" },
    });
    expect(copy.find("header").text()).toContain(workbenchMessage("en", "transferKind.copyDir"));
    expect(copy.text()).not.toContain(workbenchMessage("en", "syncDirBody"));
    copy.unmount();
  });

  it("packs patterns, clamps counts and skips empty fields on confirm", async () => {
    const wrapper = dialog();
    await wrapper.find('input[placeholder="/mirror/2024"]').setValue("/mirror/2025");
    await wrapper.find('input[placeholder="*.jpg, reports/*"]').setValue(" *.jpg , .keep, reports/* ");
    await wrapper.find('input[placeholder="*.tmp, .DS_Store"]').setValue("*.tmp, ,  ");
    await wrapper.find('input[placeholder="_backups"]').setValue("_backups/2024");
    await wrapper.find('input[placeholder=".bak"]').setValue(".bak");
    // 高级区先展开再填写；越界值夹紧（transfers 上限 32，retries 下限 1）。
    await wrapper.get(".wb-sync-advanced-toggle").trigger("click");
    await wrapper.find('input[placeholder="4"]').setValue("999");
    await wrapper.find('input[placeholder="8"]').setValue("abc");
    await wrapper.find('input[placeholder="3"]').setValue("0");
    await wrapper.find('input[type="checkbox"]').setValue(true);

    await wrapper.find(".wb-dialog-primary").trigger("click");
    const payload = wrapper.emitted("confirm")?.[0]?.[0] as Record<string, unknown>;
    expect(payload).toEqual({
      targetPath: "/mirror/2025",
      dryRun: true,
      include: ["*.jpg", ".keep", "reports/*"],
      exclude: ["*.tmp"],
      backupDir: "_backups/2024",
      suffix: ".bak",
      transfers: 32,
      checkers: null,
      retries: 1,
    });
    wrapper.unmount();
  });

  it("disables confirm while the target path is blank", async () => {
    const wrapper = dialog();
    await wrapper.find('input[placeholder="/mirror/2024"]').setValue("   ");
    expect(wrapper.find(".wb-dialog-primary").attributes("disabled")).toBeDefined();
    await wrapper.find(".wb-dialog-primary").trigger("click");
    expect(wrapper.emitted("confirm")).toBeUndefined();
    // 取消按钮 → close。
    await wrapper.find(".wb-dialog-cancel").trigger("click");
    expect(wrapper.emitted("close")).toHaveLength(1);
    wrapper.unmount();
  });
});
