// @vitest-environment happy-dom
// 跨栏拖放 copy/move 选择（rclone-ui parity）集成冒烟：内部拖放（x-dbx-files）
// 先弹「复制/移动」选择，确认后才走既有 copy/move 链路（冲突预检复用）；取消
// 不执行；同栏拖放忽略；OS 文件拖入（上传）不受影响。真实 App + mock 桥。
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

async function openDualPane() {
  const button = wrapper!.getComponent(FileToolbar).findAll("button")
    .find((button) => button.attributes("aria-label") === workbenchMessage("en", "dualPane"))!;
  await button.trigger("click");
  await settle();
}

/** 记录 copy/move 调用，其余方法（list 等）照常走 mock 宿主。 */
function stubTransfers() {
  const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
  const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
  vi.spyOn(window.dbxPlugin, "invoke").mockImplementation((async (
    method: string,
    params?: Record<string, unknown>,
    options?: { timeoutMs?: number },
  ) => {
    if (method === "files/copy" || method === "files/move") {
      calls.push({ method, params: params ?? {} });
      return { success: true };
    }
    return raw(method, params, options);
  }) as typeof window.dbxPlugin.invoke);
  return calls;
}

function internalDropEvent(paneId: "left" | "right", paths: string[]) {
  const event = new Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "dataTransfer", {
    value: {
      getData: (type: string) => (type === "application/x-dbx-files" ? JSON.stringify({ paneId, paths }) : ""),
      items: [],
      files: [],
    },
  });
  return event;
}

function paneTable(paneId: "left" | "right") {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === paneId)!;
}

/** 源栏当前列表首条目（动态取，避免与 mock 本地树/主目录起点耦合）。 */
function firstLeftEntry(): FileEntry {
  return (paneTable("left").props("entries") as FileEntry[])[0];
}

function chooser() {
  return wrapper!.find(".wb-dropaction-options");
}

function confirmButton() {
  return wrapper!.get(".wb-dialog .wb-dialog-primary");
}

describe("cross-pane drop action chooser", () => {
  it("opens the chooser first and copies only after confirming copy", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const calls = stubTransfers();
    const dragPath = firstLeftEntry().path;
    const targetPath = `/${dragPath.split("/").pop()}`;
    wrapper!.get(".wb-pane-target").element.dispatchEvent(internalDropEvent("left", [dragPath]));
    await settle();
    // 选择器先弹，确认前不执行任何 copy/move。
    expect(chooser().exists()).toBe(true);
    expect(chooser().text()).toContain(workbenchMessage("en", "dropActionCopy"));
    expect(calls).toEqual([]);
    await confirmButton().trigger("click");
    await settle();
    expect(chooser().exists()).toBe(false);
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({
      method: "files/copy",
      params: {
        sourcePath: dragPath,
        targetPath,
        sourceConnectionId: "__local__",
        targetConnectionId: "mock-conn",
      },
    });
    expect(calls[0].method).not.toBe("files/move");
  });

  it("moves instead when the move option is selected", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const calls = stubTransfers();
    const dragPath = firstLeftEntry().path;
    wrapper!.get(".wb-pane-target").element.dispatchEvent(internalDropEvent("left", [dragPath]));
    await settle();
    // 方向键切到「移动」再确认。
    const options = wrapper!.findAll(".wb-dropaction-option");
    await options[0].trigger("keydown", { key: "ArrowDown" });
    expect(options[1].classes()).toContain("is-active");
    await confirmButton().trigger("click");
    await settle();
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("files/move");
    expect(calls[0].params).toMatchObject({ sourcePath: dragPath, targetConnectionId: "mock-conn" });
  });

  it("runs nothing when the chooser is cancelled (Esc)", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const calls = stubTransfers();
    wrapper!.get(".wb-pane-target").element.dispatchEvent(internalDropEvent("left", [firstLeftEntry().path]));
    await settle();
    expect(chooser().exists()).toBe(true);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await settle();
    expect(chooser().exists()).toBe(false);
    expect(calls).toEqual([]);
  });

  it("ignores drops within the same pane", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    stubTransfers();
    wrapper!.get(".wb-pane-source").element.dispatchEvent(internalDropEvent("left", [firstLeftEntry().path]));
    await settle();
    expect(chooser().exists()).toBe(false);
  });

  it("keeps OS file drag-in uploading without the chooser", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const calls = stubTransfers();
    const event = new Event("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "dataTransfer", { value: { getData: () => "", items: [], files: [new File(["A"], "os-drop.txt")] } });
    wrapper!.get(".wb-pane-target").element.dispatchEvent(event);
    await settle();
    expect(chooser().exists()).toBe(false);
    expect(calls).toEqual([]);
    expect(vi.mocked(window.dbxPlugin.invoke).mock.calls.find(([method]) => method === "files/upload/start")).toBeTruthy();
  });
});
