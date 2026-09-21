// @vitest-environment happy-dom
// 打开方式 UI 冒烟（远程编辑本地副本，FinalShell 式）：文件右键「Open with…」
// → 选择应用对话框（预设 chips / 手输路径 / 系统默认）→ files/remote-edit/open
// （app 透传或缺省）；sidecar files/remote-edit/state 事件 opened/synced 顶部
// 提示、error 错误条。真实 App + mock 桥，不触达真实文件或应用。
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
let host: NonNullable<ReturnType<typeof installMockHost>>;

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
  // 「打开方式」仅桌面端（canSaveLocal）可见：mock 需 local=1。
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1&platform=macos");
  host = installMockHost()!;
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

// stubInvoke 按方法名分流：对话框确认前会先走 validate-open-app（mock 恒
// 通过），不能让它消费掉 spy 的 open 结果。
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

const fileEntry: FileEntry = {
  name: "report.txt",
  path: "/home/demo/report.txt",
  kind: "file",
  size: 12,
  modifiedAt: new Date().toISOString(),
};

function openWithCall(spy: ReturnType<typeof stubInvoke>) {
  const entry = spy.mock.calls.find(([method]) => method === "files/remote-edit/open");
  return entry?.[1] as Record<string, unknown> | undefined;
}

describe("open-with remote edit", () => {
  it("opens the dialog from the context menu and calls remote-edit/open with the system default app", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/remote-edit/open" ? { key: "k1", localPath: "/tmp/edit/report.txt" } : undefined,
    );
    await openEntryMenu(fileEntry);
    await menuItem(workbenchMessage("en", "openWithMenu"))!.trigger("click");
    await settle();
    // 对话框先出现：macos 预设 chips + 手输路径框，尚未发起 open。
    expect(wrapper!.find(".wb-dialog").exists()).toBe(true);
    expect(wrapper!.findAll(".wb-preset-chip").length).toBeGreaterThan(0);
    expect(openWithCall(spy)).toBeUndefined();
    // 输入留空 = 系统默认应用（不带 app 参数）。
    await wrapper!.find(".wb-dialog .wb-dialog-primary").trigger("click");
    await settle();
    const params = openWithCall(spy);
    expect(params?.remotePath).toBe(fileEntry.path);
    expect(params?.app).toBeUndefined();
    expect(wrapper!.find(".wb-notice").text()).toContain(workbenchMessage("en", "openWithOpening"));
  });

  it("forwards a chosen preset as the app parameter", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/remote-edit/open" ? { key: "k2", localPath: "/tmp/edit/report.txt" } : undefined,
    );
    await openEntryMenu(fileEntry);
    await menuItem(workbenchMessage("en", "openWithMenu"))!.trigger("click");
    await settle();
    await wrapper!.findAll(".wb-preset-chip")[0]!.trigger("click");
    await wrapper!.find(".wb-dialog .wb-dialog-primary").trigger("click");
    await settle();
    const params = openWithCall(spy);
    expect(params?.app).toBe("/Applications/wpsoffice.app");
  });

  it("blocks confirm when the custom app path fails validation", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) => {
      if (method === "files/local/validate-open-app") throw new Error("External app is not accessible: nope");
      return undefined;
    });
    await openEntryMenu(fileEntry);
    await menuItem(workbenchMessage("en", "openWithMenu"))!.trigger("click");
    await settle();
    const input = wrapper!.find(".wb-dialog input");
    ;(input.element as HTMLInputElement).value = "/nope/app";
    await input.trigger("input");
    await wrapper!.find(".wb-dialog .wb-dialog-primary").trigger("click");
    await settle();
    // 校验失败：错误就地展示，open 不发起，对话框保持打开。
    expect(wrapper!.find(".wb-dialog .wb-dialog-warning").text()).toContain("not accessible");
    expect(openWithCall(spy)).toBeUndefined();
    expect(wrapper!.find(".wb-dialog").exists()).toBe(true);
  });

  it("surfaces sidecar remote-edit/state events as notices and an error banner", async () => {
    mountWorkbench();
    await settle();
    host.emitEvent("files/remote-edit/state", {
      key: "k1",
      connectionId: "mock-conn",
      remotePath: "/docs/a.txt",
      localPath: "/tmp/edit/a.txt",
      state: "opened",
    });
    await nextTick();
    expect(wrapper!.find(".wb-notice").text()).toContain(
      workbenchMessage("en", "remoteEditOpened", { name: "a.txt" }),
    );
    host.emitEvent("files/remote-edit/state", {
      key: "k1",
      connectionId: "mock-conn",
      remotePath: "/docs/a.txt",
      localPath: "/tmp/edit/a.txt",
      state: "synced",
    });
    await nextTick();
    expect(wrapper!.find(".wb-notice").text()).toContain(
      workbenchMessage("en", "remoteEditSynced", { name: "a.txt" }),
    );
    host.emitEvent("files/remote-edit/state", {
      key: "k1",
      connectionId: "mock-conn",
      remotePath: "/docs/a.txt",
      localPath: "/tmp/edit/a.txt",
      state: "error",
      error: "network down",
    });
    await nextTick();
    expect(wrapper!.get(".wb-error-banner").text()).toContain("network down");
  });
});
