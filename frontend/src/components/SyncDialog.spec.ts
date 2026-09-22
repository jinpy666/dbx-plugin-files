// @vitest-environment happy-dom
// SyncDialog 组件单测：kind 决定标题与提示、确认载荷组装（模式拆分、整数
// 夹紧、空字段不下发）、源/目标路径可编辑 + 共享目录选择器回填、路径提交
// 上抛 pair-change、目标路径为空时确认禁用、取消/关闭事件。
import { describe, expect, it } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
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
      connectionId: "conn-test",
    },
  });
}

function sourceInput(wrapper: ReturnType<typeof dialog>) {
  return wrapper.find('input[aria-label="Source"]');
}

describe("SyncDialog", () => {
  it("shows the sync title, mirror warning and the editable source path", () => {
    const wrapper = dialog();
    expect(wrapper.find("header").text()).toContain(workbenchMessage("en", "transferKind.syncDir"));
    // 源路径默认取右键目录，且是可编辑输入框（可重指源）。
    expect((sourceInput(wrapper).element as HTMLInputElement).value).toBe("/reports/2024");
    // sync 镜像警示（copyDir 无删除语义，不显示）。
    expect(wrapper.text()).toContain(workbenchMessage("en", "syncDirBody"));
    wrapper.unmount();

    const copy = mount(SyncDialog, {
      props: { t, kind: "copyDir" as const, sourcePath: "/a", defaultTarget: "/b", connectionId: "conn-test" },
    });
    expect(copy.find("header").text()).toContain(workbenchMessage("en", "transferKind.copyDir"));
    expect(copy.text()).not.toContain(workbenchMessage("en", "syncDirBody"));
    copy.unmount();
  });

  it("packs patterns, clamps counts and skips empty fields on confirm", async () => {
    const wrapper = dialog();
    await sourceInput(wrapper).setValue("/reports/2024");
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
    // 批次6条件过滤：minSize 下发、minAge 填值（两个 age 输入共用 "1d"
    // 占位符，find 取第一个 = minAge）；maxSize/maxAge 留空 = 不下发。
    await wrapper.find('input[placeholder="100k"]').setValue("50M");
    await wrapper.find('input[placeholder="1d"]').setValue("1h");
    await wrapper.find('input[type="checkbox"]').setValue(true);
    // 第二个 checkbox 是高级区的「保留元数据」。
    await wrapper.findAll('input[type="checkbox"]')[1].setValue(true);

    await wrapper.find(".wb-dialog-primary").trigger("click");
    const payload = wrapper.emitted("confirm")?.[0]?.[0] as Record<string, unknown>;
    expect(payload).toEqual({
      sourcePath: "/reports/2024",
      targetPath: "/mirror/2025",
      dryRun: true,
      include: ["*.jpg", ".keep", "reports/*"],
      exclude: ["*.tmp"],
      backupDir: "_backups/2024",
      suffix: ".bak",
      metadata: true,
      update: false,
      existing: false,
      immutable: false,
      minSize: "50M",
      maxSize: "",
      minAge: "1h",
      maxAge: "",
      transfers: 32,
      checkers: null,
      retries: 1,
      bisyncResync: false,
    });
    wrapper.unmount();
  });

  it("carries the three policy flags as true when checked", async () => {
    const wrapper = dialog();
    // rclone 策略旗标复选框与「保留元数据」同处高级区：展开后依次是
    // dryRun / 元数据 / update / existing / immutable。
    await wrapper.get(".wb-sync-advanced-toggle").trigger("click");
    const labels = wrapper.findAll(".wb-sync-grid .wb-sync-check").map((label) => label.text());
    expect(labels).toContain(workbenchMessage("en", "syncUpdateLabel"));
    expect(labels).toContain(workbenchMessage("en", "syncExistingLabel"));
    expect(labels).toContain(workbenchMessage("en", "syncImmutableLabel"));
    const boxes = wrapper.findAll('input[type="checkbox"]');
    await boxes[2].setValue(true);
    await boxes[3].setValue(true);
    await boxes[4].setValue(true);
    await wrapper.find(".wb-dialog-primary").trigger("click");
    const payload = wrapper.emitted("confirm")?.[0]?.[0] as Record<string, unknown>;
    expect(payload).toMatchObject({ update: true, existing: true, immutable: true, metadata: false });
    wrapper.unmount();
  });

  it("picks source and target paths through the shared directory browser", async () => {
    window.dbxPlugin = {
      invoke: (method: string) => {
        if (method === "files/list") {
          return Promise.resolve({
            entries: [{ path: "/reports/sub", name: "sub", kind: "directory", size: 0, modifiedAt: new Date().toISOString() }],
          });
        }
        return Promise.resolve({});
      },
    } as unknown as Window["dbxPlugin"];
    const wrapper = dialog();
    const browseButtons = wrapper.findAll(".wb-sync-browse");
    expect(browseButtons).toHaveLength(2);
    // 打开源字段选择器：内嵌浏览器出现，从源路径父目录起步（stub 不校验）。
    await browseButtons[0].trigger("click");
    expect(wrapper.find(".wb-mount-list").exists()).toBe(true);
    await flushPromises();
    // 进入子目录 = 选中：导航结果回填源输入框。
    await wrapper.find(".wb-mount-dir").trigger("click");
    await flushPromises();
    expect((sourceInput(wrapper).element as HTMLInputElement).value).toBe("/reports/sub");
    // 再点同一按钮收起；换绑目标字段后导航回填目标输入框。
    await browseButtons[0].trigger("click");
    expect(wrapper.find(".wb-mount-list").exists()).toBe(false);
    await browseButtons[1].trigger("click");
    await wrapper.find(".wb-mount-dir").trigger("click");
    await flushPromises();
    const target = wrapper.find('input[aria-label="Target path"]');
    expect((target.element as HTMLInputElement).value).toBe("/mirror/sub");
    wrapper.unmount();
    Reflect.deleteProperty(window, "dbxPlugin");
  });

  it("emits pair-change when a path is committed", async () => {
    const wrapper = dialog();
    const source = sourceInput(wrapper);
    await source.setValue("/reports/2025");
    await source.trigger("change");
    expect(wrapper.emitted("pair-change")?.[0]?.[0]).toEqual({
      sourcePath: "/reports/2025",
      targetPath: "/mirror/2024",
    });
    wrapper.unmount();
  });

  it("shows backup and advanced hints only outside bisync mode", () => {
    const sync = dialog();
    expect(sync.text()).toContain(workbenchMessage("en", "syncBackupDirHint"));
    expect(sync.text()).toContain(workbenchMessage("en", "syncAdvancedHint"));
    sync.unmount();

    const bisync = mount(SyncDialog, {
      props: { t, kind: "bisync" as const, sourcePath: "/docs", defaultTarget: "/mirror/docs", connectionId: "conn-test" },
    });
    // bisync 无过滤/备份/高级区，相关提示整段不渲染。
    expect(bisync.text()).not.toContain(workbenchMessage("en", "syncBackupDirHint"));
    expect(bisync.text()).not.toContain(workbenchMessage("en", "syncAdvancedHint"));
    bisync.unmount();
  });

  it("closes on Escape like the other top-level dialogs", async () => {
    const wrapper = dialog();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(wrapper.emitted("close")).toHaveLength(1);
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

  it("blocks and warns when the target equals the source", async () => {
    const wrapper = dialog();
    // 目标改成与源相同 → 红色警告 + 确认禁用（不等 rc 报错）。
    await wrapper.find('input[placeholder="/mirror/2024"]').setValue("/reports/2024");
    expect(wrapper.text()).toContain(workbenchMessage("en", "destMustDiffer"));
    expect(wrapper.find(".wb-dialog-primary").attributes("disabled")).toBeDefined();
    await wrapper.find(".wb-dialog-primary").trigger("click");
    expect(wrapper.emitted("confirm")).toBeUndefined();
    // 改回不同路径 → 恢复可用，警告消失。
    await wrapper.find('input[placeholder="/mirror/2024"]').setValue("/mirror/2024");
    expect(wrapper.text()).not.toContain(workbenchMessage("en", "destMustDiffer"));
    expect(wrapper.find(".wb-dialog-primary").attributes("disabled")).toBeUndefined();
    wrapper.unmount();
  });
});

describe("SyncDialog bisync mode", () => {
  function bisyncDialog(state: "synced" | "new" | null) {
    return mount(SyncDialog, {
      props: {
        t,
        kind: "bisync" as const,
        sourcePath: "/docs",
        defaultTarget: "/mirror/docs",
        bisyncState: state,
        connectionId: "conn-test",
      },
    });
  }

  it("shows the beta warning; first runs force resync on and locked", async () => {
    const first = bisyncDialog("new");
    expect(first.text()).toContain(workbenchMessage("en", "bisyncBetaWarn"));
    expect(first.text()).toContain(workbenchMessage("en", "bisyncFirstRun"));
    const checkbox = first.find('input[type="checkbox"]');
    expect((checkbox.element as HTMLInputElement).checked).toBe(true);
    expect(checkbox.attributes("disabled")).toBeDefined();
    await first.find(".wb-dialog-primary").trigger("click");
    const payload = first.emitted("confirm")?.[0]?.[0] as Record<string, unknown>;
    expect(payload.bisyncResync).toBe(true);
    first.unmount();

    // 已有状态：resync 默认关、可勾选。
    const incremental = bisyncDialog("synced");
    const box = incremental.find('input[type="checkbox"]');
    expect((box.element as HTMLInputElement).checked).toBe(false);
    expect(box.attributes("disabled")).toBeUndefined();
    incremental.unmount();
  });
})
