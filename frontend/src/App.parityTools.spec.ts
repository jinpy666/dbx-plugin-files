// @vitest-environment happy-dom
// parity-tools 批次 UI 冒烟：目录右键「打包下载」→ files/archiveDownload →
// 传输面板归档任务；多选批量重命名（底部按钮/右键）→ 批量 rename；快捷键
// 速查（`?` / 空白区右键入口 / Esc 关闭）。真实 App + mock 桥，不触真实后端。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import TransferPanel from "./components/TransferPanel.vue";
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

function menuItem(label: string) {
  return wrapper!.findAll("[role=menuitem]").find((item) => item.text() === label);
}

function leftEntry(target: string): FileEntry {
  return table().props("entries").find((item) => item.path === target)!;
}

function stubInvoke(handler: (method: string, params: Record<string, unknown>) => unknown) {
  const raw = window.dbxPlugin!.invoke.bind(window.dbxPlugin);
  const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
  const spy = vi.spyOn(window.dbxPlugin!, "invoke").mockImplementation(((method: string, params?: Record<string, unknown>) => {
    const payload = (params ?? {}) as Record<string, unknown>;
    const routed = handler(method, payload);
    // 命中桩的方法短路返回（不透传 mock）；其余走原始 mock 桥。
    if (routed !== undefined) {
      calls.push({ method, params: payload });
      return Promise.resolve(routed) as ReturnType<typeof window.dbxPlugin.invoke>;
    }
    return raw(method, payload) as ReturnType<typeof window.dbxPlugin.invoke>;
  }) as typeof window.dbxPlugin.invoke);
  return { spy, calls };
}

// ---- 快捷键速查（Task 4）------------------------------------------------------
describe("shortcuts help overlay", () => {
  it("opens on ? without input focus and closes on Escape", async () => {
    mountWorkbench();
    await settle();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "?" }));
    await settle();
    const overlay = wrapper!.find(".wb-shortcuts");
    expect(overlay.exists()).toBe(true);
    expect(overlay.text()).toContain(workbenchMessage("en", "shortcutsGroupBrowse"));
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await settle();
    expect(wrapper!.find(".wb-shortcuts").exists()).toBe(false);
  });

  it("does not open on ? while typing in a text field", async () => {
    mountWorkbench();
    await settle();
    const input = document.createElement("input");
    document.body.appendChild(input);
    input.focus();
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "?", bubbles: true }));
    await settle();
    expect(wrapper!.find(".wb-shortcuts").exists()).toBe(false);
    input.remove();
  });

  it("opens from the blank-area context menu", async () => {
    mountWorkbench();
    await settle();
    table().vm.$emit("blank-context", { x: 5, y: 5 });
    await settle();
    await menuItem(workbenchMessage("en", "shortcutsMenu"))!.trigger("click");
    await settle();
    expect(wrapper!.find(".wb-shortcuts").exists()).toBe(true);
  });
});

// ---- 目录打包下载（Task 2）------------------------------------------------------
describe("archive download wiring", () => {
  it("starts files/archiveDownload from the directory menu and tracks the archive transfer", async () => {
    mountWorkbench();
    await settle();
    const { calls } = stubInvoke((method) => (method === "files/archiveDownload" ? { taskId: "task-arch-1" } : undefined));
    table().vm.$emit("contextmenu", { entry: leftEntry("/docs"), x: 10, y: 10 });
    await settle();
    await menuItem(workbenchMessage("en", "archiveDownloadMenu"))!.trigger("click");
    await settle();
    expect(calls).toEqual([
      { method: "files/archiveDownload", params: { connectionId: "mock-conn", path: "/docs" } },
    ]);
    // 任务登记进既有传输面板（kind=archiveDownload，标签为目录名）。
    await wrapper!.findComponent(FileToolbar).vm.$emit("toggle-dock", "transfers");
    await vi.advanceTimersByTimeAsync(500);
    await settle();
    const panel = wrapper!.findComponent(TransferPanel);
    expect(panel.exists()).toBe(true);
    const job = (panel.props("jobs") as Array<{ jobId: string; kind: string; remotePath?: string }>).find((item) => item.jobId === "task-arch-1");
    expect(job).toMatchObject({ kind: "archiveDownload", remotePath: "/docs" });
    expect(panel.text()).toContain(workbenchMessage("en", "transferKind.archiveDownload"));
  });

  it("offers pack-and-download in the multi-select menu when all selected are directories", async () => {
    mountWorkbench();
    await settle();
    table().vm.$emit("update:selection", ["/docs", "/media"]);
    await settle();
    table().vm.$emit("contextmenu", { entry: leftEntry("/docs"), x: 10, y: 10 });
    await settle();
    const expected = workbenchMessage("en", "archiveDownloadSelected", { count: 2 });
    expect(menuItem(expected)).toBeTruthy();
    // 混选（含文件）时不出现打包下载批量项。
    table().vm.$emit("update:selection", ["/docs", "/backup.zip"]);
    await settle();
    table().vm.$emit("contextmenu", { entry: leftEntry("/docs"), x: 10, y: 10 });
    await settle();
    expect(menuItem(expected)).toBeUndefined();
  });
});

// ---- 批量重命名（Task 3）--------------------------------------------------------
describe("batch rename drawer wiring", () => {
  it("opens from the multi-select footer button and applies sequential renames", async () => {
    mountWorkbench();
    await settle();
    const { calls } = stubInvoke((method) =>
      method === "files/rename" ? { success: true, transport: "native", jobId: null } : undefined,
    );
    table().vm.$emit("update:selection", ["/docs", "/empty"]);
    await settle();
    // 键盘可达入口：FileTable 底部按钮（多选时出现）。
    const button = table().get("[data-test=batch-rename]");
    await button.trigger("click");
    await settle();
    const drawer = wrapper!.find(".wb-batch-drawer");
    expect(drawer.exists()).toBe(true);
    // docs → d0cs；empty 不命中，计划不变。
    await drawer.get("[data-test=find]").setValue("o");
    await drawer.get("[data-test=replace]").setValue("0");
    await drawer.get("[data-test=apply]").trigger("click");
    await vi.advanceTimersByTimeAsync(500);
    await settle();
    expect(calls.filter((item) => item.method === "files/rename").map((item) => item.params)).toEqual([
      { connectionId: "mock-conn", path: "/docs", newPath: "/d0cs" },
    ]);
    expect(wrapper!.get(".wb-notice").text()).toContain(workbenchMessage("en", "batchRenameApplied", { ok: 1, total: 1 }));
  });

  it("exposes batch rename in the multi-select context menu and disables it read-only", async () => {
    mountWorkbench();
    await settle();
    table().vm.$emit("update:selection", ["/docs", "/empty"]);
    await settle();
    table().vm.$emit("contextmenu", { entry: leftEntry("/docs"), x: 10, y: 10 });
    await settle();
    expect(menuItem(workbenchMessage("en", "batchRenameMenu"))!.attributes("disabled")).toBeUndefined();
    wrapper!.unmount();
    wrapper = undefined;
    Reflect.deleteProperty(window, "dbxPlugin");
    // 只读态（?ro=1）：菜单项与底部按钮均禁用（与删除/重命名同一门禁形态）。
    window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&ro=1");
    installMockHost();
    mountWorkbench();
    await settle();
    table().vm.$emit("update:selection", ["/docs", "/empty"]);
    await settle();
    table().vm.$emit("contextmenu", { entry: leftEntry("/docs"), x: 10, y: 10 });
    await settle();
    expect(menuItem(workbenchMessage("en", "batchRenameMenu"))!.attributes("disabled")).toBeDefined();
    expect((table().get("[data-test=batch-rename]").element as HTMLButtonElement).disabled).toBe(true);
  });
});
