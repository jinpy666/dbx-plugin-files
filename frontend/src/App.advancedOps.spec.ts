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

  it("opens the two-way sync dialog and starts a bisync job", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) => {
      if (method === "files/bisync/state") return { session: "mock-pair", state: "synced" };
      if (method === "files/bisync/start") return { jobId: "job-bisync-1" };
      return undefined;
    });
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "bisyncMenu"))!.trigger("click");
    await settle();
    const dialog = () => wrapper!.find(".wb-sync-dialog");
    expect(dialog().exists()).toBe(true);
    expect(dialog().text()).toContain(workbenchMessage("en", "bisyncBetaWarn"));
    // 已有状态：resync 默认关，确认时也不带 mode=resync。
    expect(spy).toHaveBeenCalledWith("files/bisync/state", expect.objectContaining({ sourcePath: "/docs" }), undefined);
    await dialog().find(".wb-dialog-primary").trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/bisync/start",
      expect.objectContaining({ sourcePath: "/docs", targetPath: expect.any(String) }),
      undefined,
    );
    const params = spy.mock.calls.find(([method]) => method === "files/bisync/start")?.[1] ?? {};
    expect(params).not.toHaveProperty("mode");
    expect(wrapper!.find(".wb-notice").text()).toContain(workbenchMessage("en", "bisyncStarted"));
  });

  it("deep-searches on Enter from the toolbar search box and navigates on click", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/search"
        ? { entries: [{ path: "/docs/report-2024.txt", size: 12, modifiedAt: new Date().toISOString() }], truncated: false, scanned: 1 }
        : undefined,
    );
    const input = wrapper!.find(".wb-search-input");
    await input.setValue("report");
    await input.trigger("keydown.enter");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/search",
      expect.objectContaining({ pattern: "report", root: expect.any(String) }),
      undefined,
    );
    const panel = wrapper!.find(".wb-deepsearch");
    expect(panel.exists()).toBe(true);
    expect(panel.text()).toContain("/docs/report-2024.txt");
    await panel.findAll("li button")[0]!.trigger("click");
    await settle();
    expect(wrapper!.find(".wb-deepsearch").exists()).toBe(false);
  });

  it("imports a file from a URL into a directory via the confirm dialog", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/copyurl" ? { path: "/docs/logo.png", filename: "logo.png" } : undefined,
    );
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "copyurlMenu"))!.trigger("click");
    await settle();
    expect(spy).not.toHaveBeenCalledWith("files/copyurl", expect.anything(), undefined);
    const input = wrapper!.find("[role=dialog] input");
    await input.setValue("https://example.com/img/logo.png");
    await wrapper!.find("[role=dialog] .wb-dialog-primary").trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/copyurl",
      expect.objectContaining({ dirPath: "/docs", url: "https://example.com/img/logo.png" }),
      undefined,
    );
    expect(wrapper!.find(".wb-notice").text()).toContain("logo.png");
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

  it("starts an HTTP share from the directory context menu", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/serve/start"
        ? { serveId: "http-abc123", url: "http://127.0.0.1:41234", serveType: "http" }
        : undefined,
    );
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "shareHttpMenu"))!.trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/serve/start",
      expect.objectContaining({ path: "/docs", serveType: "http" }),
      undefined,
    );
    expect(wrapper!.find(".wb-notice").text()).toContain("http://127.0.0.1:41234");
  });

  it("renders the local shares block in the mounts settings category", async () => {
    // 挂载分类仅桌面端可见：测试内重装 mock（?local=1），再挂载 workbench。
    Reflect.deleteProperty(window, "dbxPlugin");
    window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1");
    installMockHost();
    const spy = stubInvoke((method) =>
      method === "files/serve/list"
        ? { serves: [{ serveId: "http-abc123", url: "http://127.0.0.1:41234", serveType: "webdav" }] }
        : undefined,
    );
    mountWorkbench();
    await settle();
    await toolbarButton(workbenchMessage("en", "settings"))!.trigger("click");
    await navButton(workbenchMessage("en", "settingsNav.mounts"))!.trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith("files/serve/list", expect.objectContaining({ connectionId: expect.any(String) }), undefined);
    const shares = wrapper!.find(".wb-shares-title");
    expect(shares.exists()).toBe(true);
    expect(shares.text()).toContain(workbenchMessage("en", "shareSectionTitle"));
    expect(wrapper!.text()).toContain("http://127.0.0.1:41234");
    // 停止按钮（v-tip 同步 aria-label）调 files/serve/stop。
    const stop = wrapper!
      .findAll("button")
      .find((button) => button.attributes("aria-label") === workbenchMessage("en", "shareStop"));
    expect(stop).toBeTruthy();
    await stop!.trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith("files/serve/stop", expect.objectContaining({ serveId: "http-abc123" }), undefined);
  });

  function toolbarButton(label: string) {
    return wrapper!
      .get(".wb-toolbar-actions")
      .findAll("button")
      .find((button) => button.attributes("aria-label") === label);
  }

  function navButton(label: string) {
    return wrapper!
      .get(".wb-settings-nav")
      .findAll("button")
      .find((button) => button.text() === label);
  }
});
