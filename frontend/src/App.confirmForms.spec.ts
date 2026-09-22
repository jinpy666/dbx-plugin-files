// @vitest-environment happy-dom
// 右键操作表单（ConfirmDialog 系）交互：按 kind 的字段标签与占位、空草稿
// 禁用确认、压缩后缀行内校验、路径目标的目录选择器回填。
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

function dialog() {
  return wrapper!.find("[role=dialog]");
}

function inputValue() {
  return (dialog().find("input").element as HTMLInputElement).value;
}

const dirEntry: FileEntry = {
  name: "docs",
  path: "/docs",
  kind: "directory",
  size: 0,
  modifiedAt: new Date().toISOString(),
};

describe("right-click confirm forms", () => {
  it("rename form shows the new-name label, prefills and disables on empty draft", async () => {
    mountWorkbench();
    await settle();
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "rename"))!.trigger("click");
    await settle();

    expect(dialog().text()).toContain(workbenchMessage("en", "renameNewLabel"));
    expect(inputValue()).toBe("docs");
    // 清空草稿 → 确认禁用（不再静默无操作）。
    await dialog().find("input").setValue("   ");
    expect(dialog().find(".wb-dialog-primary").attributes("disabled")).toBeDefined();
    await dialog().find(".wb-dialog-primary").trigger("click");
    // Esc 关闭（App 全局 Escape 链在 document 上）。
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await settle();
    expect(dialog().exists()).toBe(false);
  });

  it("compress form validates the archive suffix inline and disables a bad target", async () => {
    mountWorkbench();
    await settle();
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "compress"))!.trigger("click");
    await settle();

    expect(dialog().text()).toContain(workbenchMessage("en", "compressTargetLabel"));
    expect(dialog().text()).toContain(workbenchMessage("en", "compressSuffixHint"));
    // 默认草稿 /docs.tar.gz 合法可用。
    expect(inputValue()).toBe("/docs.tar.gz");
    expect(dialog().find(".wb-dialog-primary").attributes("disabled")).toBeUndefined();
    // 非法后缀 → 行内警告 + 禁用。
    await dialog().find("input").setValue("/docs.7z");
    expect(dialog().text()).toContain(workbenchMessage("en", "compressSuffixInvalid"));
    expect(dialog().find(".wb-dialog-primary").attributes("disabled")).toBeDefined();
    // 合法后缀恢复可用，警告消失。
    await dialog().find("input").setValue("/docs.zip");
    expect(dialog().find(".wb-dialog-primary").attributes("disabled")).toBeUndefined();
    expect(dialog().text()).not.toContain(workbenchMessage("en", "compressSuffixInvalid"));
  });

  it("compress picker keeps the file name when navigating into a directory", async () => {
    mountWorkbench();
    await settle();
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((method: string, params?: Record<string, unknown>) => {
      if (method === "files/list") {
        return {
          entries: [{ path: "/bucket-2024", name: "bucket-2024", kind: "directory", size: 0, modifiedAt: new Date().toISOString() }],
        };
      }
      return raw(method, params);
    }) as typeof window.dbxPlugin.invoke);
    await openEntryMenu(dirEntry);
    await menuItem(workbenchMessage("en", "compress"))!.trigger("click");
    await settle();
    // file 模式（另存为式）：进入目录后保留原文件名，而不是把目录填成目标。
    await dialog().find(".wb-sync-browse").trigger("click");
    await settle();
    await dialog().find(".wb-mount-dir").trigger("click");
    await settle();
    expect(inputValue()).toBe("/bucket-2024/docs.tar.gz");
    expect(dialog().find(".wb-dialog-primary").attributes("disabled")).toBeUndefined();
  });

  it("copy form embeds the directory browser and refills the target on navigation", async () => {
    mountWorkbench();
    await settle();
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(((method: string, params?: Record<string, unknown>) => {
      if (method === "files/list") {
        return {
          entries: [{ path: "/bucket-2024", name: "bucket-2024", kind: "directory", size: 0, modifiedAt: new Date().toISOString() }],
        };
      }
      return raw(method, params);
    }) as typeof window.dbxPlugin.invoke);
    await openEntryMenu(dirEntry);
    await menuItem(`${workbenchMessage("en", "transferKind.copy")}…`)!.trigger("click");
    await settle();

    expect(dialog().text()).toContain(workbenchMessage("en", "syncTargetLabel"));
    // 默认草稿在当前父目录下；浏览起点即其父目录（stub 不校验）。
    expect(inputValue()).toBe("/docs-copy");
    await dialog().find(".wb-sync-browse").trigger("click");
    await settle();
    await dialog().find(".wb-mount-dir").trigger("click");
    await settle();
    expect(inputValue()).toBe("/bucket-2024");
    // 再点同一按钮收起浏览器，输入仍保留所选值。
    await dialog().find(".wb-sync-browse").trigger("click");
    await settle();
    expect(dialog().find(".wb-mount-list").exists()).toBe(false);
    expect(inputValue()).toBe("/bucket-2024");
  });
});
