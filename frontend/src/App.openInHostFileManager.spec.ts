// @vitest-environment happy-dom
// 「在 DBX 中打开」入口冒烟（真实 App + mock 桥，filesystem-provider 交接）：
// 能力探测（旧宿主无 openFilesystem → 按钮不渲染）、点击以正确 providerId/uri
// 交接活动栏当前目录、本地窗格禁用且 tooltip 说明仅远端支持。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import { installMockHost, mockOpenFilesystemCalls } from "./lib/mockHost";
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

function openInHostButton() {
  return wrapper!.find(".wb-open-in-host-toggle");
}

function leftTable() {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === "left")!;
}

const docsEntry: FileEntry = {
  name: "docs",
  path: "/docs",
  kind: "directory",
  size: 0,
  modifiedAt: new Date().toISOString(),
};

describe("open in host file manager toolbar entry", () => {
  it("hides the button when the host lacks openFilesystem (capability probe)", async () => {
    // 模拟旧宿主：能力方法整体缺失，入口不能渲染成一个点了没反应的死按钮。
    Reflect.deleteProperty(window.dbxPlugin, "openFilesystem");
    mountWorkbench();
    await settle();
    expect(openInHostButton().exists()).toBe(false);
  });

  it("hands the active pane directory to the host with providerId and rclone uri", async () => {
    mountWorkbench();
    await settle();
    expect(openInHostButton().attributes("aria-label")).toBe(workbenchMessage("en", "openInHostFileManager"));
    // 进入 /docs 再交接：单栏左栏 = 宿主当前连接（mock-conn）。
    leftTable().vm.$emit("open", docsEntry);
    await settle();
    await openInHostButton().trigger("click");
    await settle();
    expect(mockOpenFilesystemCalls()).toEqual([
      { providerId: "io.dbx.files.fs", context: { uri: "rclone://mock-conn/docs" } },
    ]);
  });

  it("disables the entry for the local pane and explains why in the tooltip", async () => {
    mountWorkbench();
    await settle();
    // 开双栏：左栏默认本地 __local__，工具栏目标 = 最近活动栏（默认左栏）。
    wrapper!.getComponent(FileToolbar).vm.$emit("toggle-dual-pane");
    await settle();
    const button = openInHostButton();
    expect((button.element as HTMLButtonElement).disabled).toBe(true);
    expect(button.attributes("aria-label")).toBe(workbenchMessage("en", "openInHostFileManagerLocalOnly"));
  });
});
