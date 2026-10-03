// @vitest-environment happy-dom
// 打开方式 UI 冒烟（1:1 复刻 ssh 子菜单流，FinalShell 式远程编辑）：文件右键
// 「Open with…」子菜单（系统默认 / 编辑器目录 / 自定义编辑器 / 自定义命令弹窗
// 带「记住为默认」）→ files/remote-edit/open（editorId / customCommand /
// 缺省透传）；ask 回传策略的 modified 事件 → 三档决议弹窗（总是上传/上传
// 一次/取消）→ files/remote-edit/decide；sidecar files/remote-edit/state
// 事件 opened/synced 顶部提示、error 错误条。真实 App + mock 桥，不触达真实
// 文件或应用。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import { loadEditorConfig } from "./lib/editorRules";
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

/** 展开右键「打开方式」子菜单（ssh 同形：触发项 + 悬停/点选展开）。 */
async function openSubmenu() {
  await menuItem(workbenchMessage("en", "openWithMenu"))!.trigger("click");
  await settle();
}

// stubInvoke 按方法名分流：让测试捕获目标 RPC，其余走真实 mock。
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

describe("open-with remote edit (submenu flow)", () => {
  it("lists catalog editors in the submenu and opens via the system default", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/remote-edit/open" ? { key: "k1", localPath: "/tmp/edit/report.txt" } : undefined,
    );
    await openEntryMenu(fileEntry);
    // 子菜单触发项存在但尚未展开，open 未发起。
    expect(menuItem(workbenchMessage("en", "openWithMenu"))).toBeTruthy();
    expect(openWithCall(spy)).toBeUndefined();
    await openSubmenu();
    // 编辑器目录条目（macos 目录：vscode/textedit 可用；sublime 不可用隐藏）。
    expect(menuItem("Visual Studio Code")).toBeTruthy();
    expect(menuItem("TextEdit")).toBeTruthy();
    expect(menuItem("Sublime Text")).toBeUndefined();
    // 系统默认程序：open 不带任何打开目标参数。
    await menuItem(workbenchMessage("en", "sftpEdit.systemDefault"))!.trigger("click");
    await settle();
    const params = openWithCall(spy);
    expect(params?.remotePath).toBe(fileEntry.path);
    expect(params?.editorId).toBeUndefined();
    expect(params?.customCommand).toBeUndefined();
    expect(params?.app).toBeUndefined();
    // 选中即关菜单（ssh 同形：一次性打开，不写关联）。
    expect(wrapper!.find("[role=menu]").exists()).toBe(false);
  });

  it("forwards a chosen catalog editor as the editorId parameter", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/remote-edit/open" ? { key: "k2", localPath: "/tmp/edit/report.txt" } : undefined,
    );
    await openEntryMenu(fileEntry);
    await openSubmenu();
    await menuItem("Visual Studio Code")!.trigger("click");
    await settle();
    const params = openWithCall(spy);
    expect(params?.editorId).toBe("vscode");
    expect(params?.app).toBeUndefined();
  });

  it("saves a custom command with 'remember as default' and opens via customCommand", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/remote-edit/open" ? { key: "k3", localPath: "/tmp/edit/report.txt" } : undefined,
    );
    await openEntryMenu(fileEntry);
    await openSubmenu();
    await menuItem(workbenchMessage("en", "sftpEdit.customCommand"))!.trigger("click");
    await settle();
    // 自定义命令弹窗：命令必填，名称缺省取命令首词；记住为默认默认勾选。
    const dialog = wrapper!.find(".wb-dialog");
    expect(dialog.exists()).toBe(true);
    const inputs = dialog.findAll("input");
    await inputs[0]!.setValue("My Code");
    await inputs[1]!.setValue("code --wait {file}");
    expect((inputs[2]!.element as HTMLInputElement).checked).toBe(true);
    await wrapper!.find(".wb-dialog .wb-dialog-primary").trigger("click");
    await settle();
    const params = openWithCall(spy);
    expect(params?.customCommand).toBe("code --wait {file}");
    expect(params?.editorId).toBeUndefined();
    // 「记住为默认」写入 {pattern → customId} 关联 + 自定义编辑器（配置持久化）。
    const config = loadEditorConfig();
    expect(config.customEditors).toHaveLength(1);
    expect(config.customEditors[0]!.command).toBe("code --wait {file}");
    expect(config.associations).toEqual([{ pattern: "*.txt", customId: config.customEditors[0]!.id }]);
  });

  it("queues ask-policy modified events and decides via files/remote-edit/decide", async () => {
    mountWorkbench();
    await settle();
    const spy = stubInvoke((method) =>
      method === "files/remote-edit/decide" ? { success: true } : undefined,
    );
    host.emitEvent("files/remote-edit/state", {
      key: "k9",
      connectionId: "mock-conn",
      remotePath: "/docs/report.txt",
      localPath: "/tmp/edit/report.txt",
      state: "modified",
    });
    await nextTick();
    // 队头决议弹窗：三档 = 总是上传 / 上传一次(primary) / 遮罩取消。
    expect(wrapper!.find(".wb-dialog").exists()).toBe(true);
    expect(wrapper!.find(".wb-dialog").text()).toContain(
      workbenchMessage("en", "sftpEdit.modifiedMessage", { name: "report.txt" }),
    );
    await wrapper!.find(".wb-dialog .wb-dialog-primary").trigger("click");
    await settle();
    const decide = spy.mock.calls.find(([method]) => method === "files/remote-edit/decide");
    // call() 会自动并入 connectionId（与 workbench 其余 RPC 同形）。
    expect(decide?.[1]).toMatchObject({ key: "k9", action: "upload" });
    // 决议后出队：弹窗关闭；后续保存再排队弹下一条。
    expect(wrapper!.find(".wb-dialog").exists()).toBe(false);
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
