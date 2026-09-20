// @vitest-environment happy-dom
// rclone 深度能力 UI 冒烟（真实 App + mock 桥）：目录右键 → 生成校验文件 /
// 清理空目录 / 与…比对（ConfirmDialog → files/check 作业），空白区右键 →
// 清空回收站（二次确认），侧栏渲染 files/about 占用条。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
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

/** 只拦截 assertions 关心的方法，其余走 mockHost 原实现。 */
function stubInvoke(route: (method: string) => unknown) {
  const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
  return vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
    method: string,
    params?: Record<string, unknown>,
    options?: { timeoutMs?: number },
  ) => {
    const routed = route(method) as unknown;
    if (routed !== undefined) return routed;
    return raw(method, params, options);
  }) as typeof window.dbxPlugin.invoke);
}

describe("advanced ops UI", () => {
  it("generates a checksum file from the directory context menu", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/hashsum" ? { path: "/docs.md5", hashType: "md5", files: 3 } : undefined,
    );
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "hashsumMenu"))!.trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/hashsum",
      expect.objectContaining({ path: "/docs", hashType: "md5" }),
      undefined,
    );
    expect(wrapper!.find(".wb-notice").text()).toContain("/docs.md5");
  });

  it("starts a comparison job via the confirm dialog", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/check" ? { jobId: "job-check-1" } : undefined,
    );
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "checkDirMenu"))!.trigger("click");
    await settle();
    // ConfirmDialog 先行，输入比对目标后再发起。
    expect(spy).not.toHaveBeenCalledWith("files/check", expect.anything(), undefined);
    const input = wrapper!.get(".wb-confirm input, .wb-confirm-dialog input, [role=dialog] input");
    await input.setValue("/backup/docs");
    await wrapper!.find("[role=dialog] .wb-dialog-primary").trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/check",
      expect.objectContaining({
        sourcePath: "/docs",
        targetPath: "/backup/docs",
        sourceConnectionId: expect.any(String),
        targetConnectionId: expect.any(String),
      }),
      undefined,
    );
    expect(wrapper!.find(".wb-notice").text()).toContain(workbenchMessage("en", "checkStarted"));
  });

  it("empties the remote trash from the blank context menu after confirmation", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) => (method === "files/cleanup" ? { success: true } : undefined));
    leftTable().vm.$emit("blank-context", { x: 12, y: 12 });
    await settle();
    await menuItem(workbenchMessage("en", "cleanupMenu"))!.trigger("click");
    await settle();
    // 二次确认弹窗先出现，确认前不发 files/cleanup。
    expect(spy).not.toHaveBeenCalledWith("files/cleanup", expect.anything(), undefined);
    // danger 确认走 wb-dialog-danger 样式。
    await wrapper!.find("[role=dialog] .wb-dialog-danger").trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith("files/cleanup", expect.objectContaining({ connectionId: expect.any(String) }), undefined);
    expect(wrapper!.find(".wb-notice").text()).toContain(workbenchMessage("en", "cleanupDone"));
  });

  it("renders the remote usage footer from files/about", async () => {
    mountWorkbench();
    await settle();
    // 右栏（含 SideNavPanel）仅在双栏模式下渲染。
    wrapper!.getComponent(FileToolbar).vm.$emit("toggle-dual-pane");
    await settle();
    const usage = wrapper!.find(".wb-side-usage");
    expect(usage.exists()).toBe(true);
    expect(usage.text()).toContain(workbenchMessage("en", "sideUsage"));
  });
});
