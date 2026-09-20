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

describe("mount to local UI", () => {
  it("reports the kernel mount point when rclone mount wins", async () => {
    mountWorkbench();
    await settle();
    const spy = vi
      .spyOn(window.dbxPlugin, "invoke")
      .mockResolvedValueOnce({ mountId: "m1", strategy: "rclone", mountPoint: "/home/x/dbx-files-mounts/dbxabc" });
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/mount",
      expect.objectContaining({ path: "/docs", strategy: "auto" }),
      undefined,
    );
    expect(wrapper!.get(".wb-notice").text()).toContain("Mounted read-only at");
  });

  it("copies the gateway URL when the webdav fallback answers", async () => {
    mountWorkbench();
    await settle();
    const spy = vi
      .spyOn(window.dbxPlugin, "invoke")
      .mockResolvedValueOnce({
        mountId: "m2",
        strategy: "webdav",
        gatewayUrl: "http://127.0.0.1:54321/tok/conn/",
        fallbackReason: "macFUSE not detected",
      });
    const writeText = vi.fn().mockResolvedValue(undefined);
    (window.dbxPlugin as { clipboard?: unknown }).clipboard = { writeText };
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith(
      "files/mount",
      expect.objectContaining({ path: "/docs", strategy: "auto" }),
      undefined,
    );
    expect(writeText).toHaveBeenCalledWith("http://127.0.0.1:54321/tok/conn/");
    expect(wrapper!.get(".wb-notice").text()).toContain("WebDAV gateway URL copied");
  });

  it("surfaces mount failures in the notice", async () => {
    mountWorkbench();
    await settle();
    vi.spyOn(window.dbxPlugin, "invoke").mockRejectedValueOnce(new Error("no fuse driver"));
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    expect(wrapper!.get(".wb-notice").text()).toContain("Mount failed: no fuse driver");
  });
});
