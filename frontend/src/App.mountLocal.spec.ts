// @vitest-environment happy-dom
// 本地挂载 UI 冒烟：目录右键「Mount to local」→ files/mount（auto 策略）。
// rclone 结果显示挂载点；webdav 兜底复制网关 URL；失败进 notice。真实 App +
// mock 桥，不触达真实文件或连接（docs/MOUNT.zh-CN.md M1）。
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

function table() {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === "left")!;
}

function openEntryMenu(entry: FileEntry) {
  table().vm.$emit("contextmenu", { entry, x: 10, y: 10 });
  return nextTick();
}

function menuItem(label: string) {
  return wrapper!.findAll("[role=menuitem]").find((item) => item.text() === label);
}

const dirEntry: FileEntry = {
  name: "docs",
  path: "/docs",
  kind: "directory",
  size: 0,
  modifiedAt: new Date().toISOString(),
};

// 挂载对话框流（选目录 → 确认）：菜单/工具栏入口先弹独立 MountDialog，
// 确认后才调 files/mount（含所选 mountPoint），成功后 reveal 挂载点。
// stubInvoke 按方法名分流：MountDialog 挂载时会先探测 quickPaths/浏览
// __local__ 目录，不能让它们消费掉 mockResolvedValueOnce 的 mount 结果。
function stubInvoke(handler: (method: string) => unknown) {
  const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
  return vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
    method: string,
    params?: Record<string, unknown>,
    options?: { timeoutMs?: number },
  ) => {
    const routed = handler(method) as unknown;
    if (routed !== undefined) return routed;
    return raw(method, params, options);
  }) as typeof window.dbxPlugin.invoke);
}

async function confirmMountDialog() {
  await wrapper!.find(".wb-mount-dialog .wb-dialog-primary").trigger("click");
  await settle();
}

describe("mount to local UI", () => {
  it("opens the mount dialog first and mounts the chosen directory on confirm", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/mount"
        ? { mountId: "m1", strategy: "rclone", mountPoint: "/home/x/dbx-files-mounts/dbxabc" }
        : undefined,
    );
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    // 对话框先出现，尚未发起 files/mount。
    expect(wrapper!.find(".wb-mount-dialog").text()).toContain(workbenchMessage("en", "mountToLocal"));
    expect(spy).not.toHaveBeenCalledWith("files/mount", expect.anything(), undefined);
    await confirmMountDialog();
    expect(spy).toHaveBeenCalledWith(
      "files/mount",
      expect.objectContaining({ path: "/docs", strategy: "auto" }),
      undefined,
    );
    // 挂载成功后自动在文件管理器中打开挂载点。
    expect(spy).toHaveBeenCalledWith(
      "files/local/reveal",
      expect.objectContaining({ path: "/home/x/dbx-files-mounts/dbxabc" }),
      undefined,
    );
    expect(wrapper!.get(".wb-notice").text()).toContain("Mounted read-only at");
  });

  it("passes the chosen mount point through to files/mount", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/mount" ? { mountId: "m3", strategy: "rclone", mountPoint: "/tmp/chose" } : undefined,
    );
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    await wrapper!.find(".wb-mount-field input").setValue("/tmp/chose");
    await confirmMountDialog();
    expect(spy).toHaveBeenCalledWith(
      "files/mount",
      expect.objectContaining({ path: "/docs", strategy: "auto", mountPoint: "/tmp/chose" }),
      undefined,
    );
  });

  it("copies the gateway URL when the webdav fallback answers", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/mount"
        ? {
            mountId: "m2",
            strategy: "webdav",
            gatewayUrl: "http://127.0.0.1:54321/tok/conn/",
            fallbackReason: "macFUSE not detected",
          }
        : undefined,
    );
    const writeText = vi.fn().mockResolvedValue(undefined);
    (window.dbxPlugin as { clipboard?: unknown }).clipboard = { writeText };
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    await confirmMountDialog();
    expect(spy).toHaveBeenCalledWith(
      "files/mount",
      expect.objectContaining({ path: "/docs", strategy: "auto" }),
      undefined,
    );
    expect(writeText).toHaveBeenCalledWith("http://127.0.0.1:54321/tok/conn/");
    expect(wrapper!.get(".wb-notice").text()).toContain("No FUSE driver on this machine");
  });

  it("surfaces mount failures in the error banner", async () => {
    mountWorkbench();
    await settle();
    stubInvoke((method) => (method === "files/mount" ? Promise.reject(new Error("no fuse driver")) : undefined));
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    await confirmMountDialog();
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(true);
    expect(wrapper!.find(".wb-error-banner").text()).toContain("fuse");
  });
});
