// @vitest-environment happy-dom
// 独立设置弹窗（对标 ssh 插件 settings-modal）UI 冒烟：工具栏设置 icon 打开
// 弹窗、左导航切换分类、挂载面板 files/mountStatus + files/unmount、Esc 关闭；
// 另覆盖工具栏挂载 icon 调 files/mount 与本地栏禁用态。真实 App + mock 桥。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileToolbar from "./components/FileToolbar.vue";
import SettingsPanel from "./components/SettingsPanel.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";

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
  // 挂载分类/入口仅桌面端可见：mock 需 local=1。
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1");
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

describe("standalone settings dialog", () => {
  it("opens from the toolbar settings icon and switches categories via the left nav", async () => {
    mountWorkbench();
    await settle();
    expect(wrapper!.find(".wb-settings-backdrop").exists()).toBe(false);
    await toolbarButton(workbenchMessage("en", "settings"))!.trigger("click");
    await settle();
    const backdrop = wrapper!.get(".wb-settings-backdrop");
    expect(backdrop.attributes("role")).toBe("dialog");
    expect(backdrop.attributes("aria-modal")).toBe("true");
    // 分类导航：downloads 默认选中，切到 openWith 后 SettingsPanel 只渲染对应 section。
    const panels = () => wrapper!.findAllComponents(SettingsPanel);
    expect(panels()).toHaveLength(1);
    expect(panels()[0]!.props("section")).toBe("downloads");
    await navButton(workbenchMessage("en", "settingsNav.openWith"))!.trigger("click");
    await settle();
    expect(panels()[0]!.props("section")).toBe("openWith");
  });

  it("keeps settings out of the dock tabs", async () => {
    mountWorkbench();
    await settle();
    // 打开 dock（默认 transfers）：页签只有 transfers/audit/connection 三项，
    // toolbar 的 dockTab 类型亦不再含 settings。
    wrapper!.getComponent(FileToolbar).vm.$emit("toggle-dock", "transfers");
    await settle();
    const tabs = wrapper!.get(".wb-dock-tabs").findAll('[role="tab"]');
    expect(tabs).toHaveLength(3);
    expect(tabs.map((tab) => tab.text())).toEqual([
      workbenchMessage("en", "transferPanel"),
      workbenchMessage("en", "auditPanel"),
      workbenchMessage("en", "connectionPanel"),
    ]);
  });

  it("lists mounts via files/mountStatus and unmounts from the mounts pane", async () => {
    mountWorkbench();
    await settle();
    // 批次5 起 mountStatus 后面会跟一次 files/mount/stats（best-effort 摘要），
    // stub 按方法分流而不是按次数 Once，避免摘要调用吃掉卸载后的 status 响应。
    const statuses = [
      { mounts: [{ mountId: "m1", strategy: "rclone", mountPoint: "/home/x/dbx-files-mounts/dbxabc", mounted: true }] },
    ];
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
      method: string,
    ) => {
      if (method === "files/mountStatus") return Promise.resolve(statuses.shift() ?? { mounts: [] });
      if (method === "files/mount/stats") return Promise.resolve({ mounts: [], skipped: 0 });
      return Promise.resolve({});
    }) as typeof window.dbxPlugin.invoke);
    await toolbarButton(workbenchMessage("en", "settings"))!.trigger("click");
    await navButton(workbenchMessage("en", "settingsNav.mounts"))!.trigger("click");
    await settle();
    expect(invoke).toHaveBeenCalledWith("files/mountStatus", expect.objectContaining({ connectionId: expect.any(String) }), undefined);
    const list = wrapper!.get(".wb-mounts-list");
    expect(list.text()).toContain(workbenchMessage("en", "mountStrategy.rclone"));
    expect(list.text()).toContain("/home/x/dbx-files-mounts/dbxabc");
    // 行内最后一颗按钮是「卸载」（批次5 起第一颗是「刷新缓存」）。
    await list.findAll("button").at(-1)!.trigger("click");
    await settle();
    expect(invoke).toHaveBeenCalledWith("files/unmount", expect.objectContaining({ mountId: "m1" }), undefined);
    // 卸载后 status 重拉：空列表 → 空态提示，列表消失。
    expect(wrapper!.find(".wb-mounts-list").exists()).toBe(false);
    expect(wrapper!.text()).toContain(workbenchMessage("en", "mounts.empty"));
  });

  it("closes on Escape and clears inline pref errors with it", async () => {
    mountWorkbench();
    await settle();
    await toolbarButton(workbenchMessage("en", "settings"))!.trigger("click");
    await settle();
    expect(wrapper!.find(".wb-settings-backdrop").exists()).toBe(true);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await settle();
    expect(wrapper!.find(".wb-settings-backdrop").exists()).toBe(false);
  });
});

// VFS 缓存管理（批次5）：挂载行「刷新缓存」按钮 + vfs/stats 摘要。真实 App +
// stub 桥，按方法分流（mountStatus/stats/refresh），其余走 mock 宿主。
describe("mount VFS cache management", () => {
  const rcloneRow = { mountId: "m1", strategy: "rclone", mountPoint: "/home/x/dbx-files-mounts/dbxabc", mounted: true };

  function stubMountMethods(handlers: Record<string, unknown>) {
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    return vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
      method: string,
      params?: Record<string, unknown>,
      options?: { timeoutMs?: number },
    ) => {
      const routed = handlers[method];
      if (routed !== undefined) return Promise.resolve(routed);
      return raw(method, params, options);
    }) as typeof window.dbxPlugin.invoke);
  }

  async function openMountsPane() {
    await toolbarButton(workbenchMessage("en", "settings"))!.trigger("click");
    await navButton(workbenchMessage("en", "settingsNav.mounts"))!.trigger("click");
    await settle();
    return wrapper!.get(".wb-mounts-list");
  }

  it("shows a vfs/stats summary and refreshes the cache from the rclone row", async () => {
    mountWorkbench();
    await settle();
    const invoke = stubMountMethods({
      "files/mountStatus": { mounts: [rcloneRow] },
      "files/mount/stats": {
        mounts: [
          {
            mountId: "m1",
            strategy: "rclone",
            stats: { diskCache: { bytesUsed: 2048 }, metadataCache: { dirs: 3, files: 5 } },
          },
        ],
        skipped: 0,
      },
      "files/mount/refresh": { refreshed: 1, skipped: 0, errors: [] },
    });
    const list = await openMountsPane();
    // 摘要展示缓存占用与目录数（bytes 走 formatBytes：2048 → 2.0 KiB）。
    expect(list.text()).toContain("cache 2.0 KiB");
    expect(list.text()).toContain("3 dirs");
    // 行内第一颗按钮是「刷新缓存」；点击后按 mountId 调 files/mount/refresh。
    await list.findAll("button")[0]!.trigger("click");
    await settle();
    expect(invoke).toHaveBeenCalledWith(
      "files/mount/refresh",
      expect.objectContaining({ mountId: "m1", connectionId: expect.any(String) }),
      undefined,
    );
    expect(wrapper!.get(".wb-notice").text()).toContain("Mount cache refreshed (1 refreshed, 0 skipped)");
  });

  it("reports the no-VFS notice for WebDAV gateway rows", async () => {
    mountWorkbench();
    await settle();
    stubMountMethods({
      "files/mountStatus": { mounts: [{ mountId: "m2", strategy: "webdav", gatewayPort: 41000, mounted: true }] },
      "files/mount/stats": { mounts: [], skipped: 1 },
      "files/mount/refresh": { refreshed: 0, skipped: 1, errors: [] },
    });
    const list = await openMountsPane();
    await list.findAll("button")[0]!.trigger("click");
    await settle();
    expect(wrapper!.get(".wb-notice").text()).toContain(
      workbenchMessage("en", "mountRefresh.skipped"),
    );
  });
});

describe("toolbar mount icon", () => {
  it("opens the mount dialog and mounts on confirm", async () => {
    mountWorkbench();
    await settle();
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    const spy = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((
      method: string,
      params?: Record<string, unknown>,
      options?: { timeoutMs?: number },
    ) => {
      if (method === "files/mount") {
        return { mountId: "m9", strategy: "rclone", mountPoint: "/home/x/mnt" };
      }
      return raw(method, params, options);
    }) as typeof window.dbxPlugin.invoke);
    await toolbarButton(workbenchMessage("en", "mountToLocal"))!.trigger("click");
    await settle();
    // 对话框先行：标题即挂载入口文案，确认后才发起 files/mount。
    expect(wrapper!.find(".wb-mount-dialog").text()).toContain(workbenchMessage("en", "mountToLocal"));
    expect(spy).not.toHaveBeenCalledWith("files/mount", expect.anything(), undefined);
    await wrapper!.find(".wb-mount-dialog .wb-dialog-primary").trigger("click");
    await settle();
    expect(spy).toHaveBeenCalledWith("files/mount", expect.objectContaining({ strategy: "auto", path: expect.any(String) }), undefined);
    expect(wrapper!.get(".wb-notice").text()).toContain("Mounted read-only at");
  });

  it("disables mounting while the active pane is the local connection", async () => {
    mountWorkbench();
    await settle();
    const invoke = vi.spyOn(window.dbxPlugin, "invoke");
    const toolbar = wrapper!.getComponent(FileToolbar);
    toolbar.vm.$emit("toggle-dual-pane");
    await settle();
    const mountButton = toolbarButton(workbenchMessage("en", "mountToLocal"))!;
    // 双栏开启后活动栏默认本地 __local__：挂载入口禁用且不发 files/mount。
    expect(mountButton.attributes("disabled")).toBeDefined();
    await mountButton.trigger("click");
    await settle();
    expect(invoke).not.toHaveBeenCalledWith("files/mount", expect.anything(), undefined);
  });
});
