// @vitest-environment happy-dom
// 目录同步选项流（真实 App + mock 桥）：右键目录 → Folder sync 菜单 → 独立
// SyncDialog（替代原 ConfirmDialog 流）→ 填 dry-run + 过滤 → 确认后按新参数
// 发起 files/syncDir 并登记传输作业；再验证 copyDir 菜单走同一弹窗。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import type { FileEntry } from "./lib/api";

let wrapper: VueWrapper | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  saveUiPrefs({
    sort: { column: "name", direction: "asc" },
    leftSideTab: "quick",
    rightSideTab: "tree",
    leftSideCollapsed: false,
    rightSideCollapsed: false,
  });
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0");
  installMockHost();
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "dbxPlugin");
});

function mountWorkbench() {
  wrapper = mount(App, { attachTo: document.body, global: { directives: { tip: vTip } } });
  return wrapper;
}

async function settle() {
  for (let i = 0; i < 8; i++) {
    await vi.advanceTimersByTimeAsync(1);
    await nextTick();
  }
}

function leftTable() {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === "left")!;
}

function openEntryMenu(entry: FileEntry) {
  leftTable().vm.$emit("contextmenu", { entry, x: 10, y: 10 });
  return nextTick();
}

function menuItem(label: string) {
  return wrapper!.findAll("[role=menuitem]").find((item) => item.text().startsWith(label));
}

const dirEntry: FileEntry = {
  name: "docs",
  path: "/docs",
  kind: "directory",
  size: 0,
  modifiedAt: new Date().toISOString(),
};

describe("directory sync options flow", () => {
  it("opens SyncDialog from the context menu and starts syncDir with the chosen options", async () => {
    mountWorkbench();
    await settle();
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    const spy = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
      method: string,
      params?: Record<string, unknown>,
      options?: { timeoutMs?: number },
    ) => {
      if (method === "files/syncDir") {
        return { jobId: "job-sync-1" };
      }
      return raw(method, params, options);
    }) as typeof window.dbxPlugin.invoke);

    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "transferKind.syncDir"))!.trigger("click");
    await settle();

    // 对话框先出现，确认前不发 files/syncDir。源路径是可编辑输入框（默认
    // 取右键目录），按 aria-label 断言初值。
    const dialog = () => wrapper!.find(".wb-sync-dialog");
    expect(dialog().exists()).toBe(true);
    expect((dialog().find('input[aria-label="Source"]').element as HTMLInputElement).value).toBe("/docs");
    expect(spy).not.toHaveBeenCalledWith("files/syncDir", expect.anything(), undefined);

    // 目标改为与源不同（自配对被前置拦截），dry-run + 过滤器 → 确认。
    await dialog().find('input[placeholder="/docs"]').setValue("/docs-backup");
    await dialog().find('input[type="checkbox"]').setValue(true);
    await dialog().find('input[placeholder="*.tmp, .DS_Store"]').setValue("*.tmp");
    await dialog().find(".wb-dialog-primary").trigger("click");
    await settle();

    expect(spy).toHaveBeenCalledWith(
      "files/syncDir",
      expect.objectContaining({
        sourcePath: "/docs",
        targetPath: expect.any(String),
        sourceConnectionId: expect.any(String),
        targetConnectionId: expect.any(String),
        dryRun: true,
        exclude: ["*.tmp"],
      }),
      undefined,
    );
    // 弹窗关闭、dry-run 通知出现。
    expect(dialog().exists()).toBe(false);
    expect(wrapper!.find(".wb-notice").text()).toContain(workbenchMessage("en", "syncDryRunStarted"));
  });

  it("routes copyDir through the same dialog without the mirror warning", async () => {
    mountWorkbench();
    await settle();
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    const spy = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
      method: string,
      params?: Record<string, unknown>,
      options?: { timeoutMs?: number },
    ) => {
      if (method === "files/copyDir") {
        return { jobId: "job-copy-1" };
      }
      return raw(method, params, options);
    }) as typeof window.dbxPlugin.invoke);

    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "transferKind.copyDir"))!.trigger("click");
    await settle();
    const dialog = wrapper!.find(".wb-sync-dialog");
    expect(dialog.exists()).toBe(true);
    expect(dialog.text()).not.toContain(workbenchMessage("en", "syncDirBody"));

    // 目标改为与源不同（自配对被前置拦截）再确认。
    await dialog.find('input[placeholder="/docs"]').setValue("/docs-copy");
    await dialog.find(".wb-dialog-primary").trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/copyDir",
      expect.objectContaining({ sourcePath: "/docs" }),
      undefined,
    );
    const params = spy.mock.calls.find(([method]) => method === "files/copyDir")?.[1] ?? {};
    expect(params).not.toHaveProperty("dryRun");
  });
});
