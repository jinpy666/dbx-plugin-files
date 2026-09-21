// @vitest-environment happy-dom
// 回归规格：预览 Esc 关闭后，双击同一条目必须能重新打开预览。
// 背景（followup-preview-dblclick）：走查发现 Esc 关闭后紧随的双击偶发不触发
// open——关闭时焦点可能悬空（returnFocusTo 捕获到已卸载的触发元素），首次点击
// 仅落在列表取焦上，dblclick 判定失效。锁定两层行为：① 关闭后焦点确定性归还
// 来源栏列表容器；② Esc → 双击同一条目 → 预览重开全链路。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick, type Component } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import type { FileEntry } from "./lib/api";

// 可编程的 PreviewPane 桩（与 App.freshReview 同款）：isDirty 关闭守卫接线，
// 预览渲染细节由 PreviewPane.spec 覆盖，这里只关心 App 层开关链路。
const PreviewPaneStub = {
  props: { path: { type: String, default: null }, canWrite: Boolean },
  emits: ["close", "saved", "download"],
  expose: ["isDirty"],
  setup() {
    return { isDirty: false };
  },
  template: `<div class="stub-preview" data-test="stub-preview">{{ path }}</div>`,
} as unknown as Component;

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
  wrapper = mount(App, {
    attachTo: document.body,
    global: { directives: { tip: vTip }, stubs: { PreviewPane: PreviewPaneStub } },
  });
  return wrapper;
}

async function settle() {
  for (let i = 0; i < 8; i++) {
    await vi.advanceTimersByTimeAsync(1);
    await nextTick();
  }
}

function table(side: "left" | "right") {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === side)!;
}

function rowOf(side: "left" | "right", path: string) {
  return table(side).findAll(".wb-file-row").find((row) =>
    row.find(".wb-file-name span").attributes("title") === path,
  )!;
}

async function openDualPane() {
  const button = wrapper!.getComponent(FileToolbar).findAll("button")
    .find((button) => button.attributes("aria-label") === workbenchMessage("en", "dualPane"))!;
  await button.trigger("click");
  await settle();
}

// 根目录可见的文件条目（mockHost 种子：/backup.zip）。
const rootFile: FileEntry = {
  name: "backup.zip", path: "/backup.zip", kind: "file", size: 1, modifiedAt: new Date().toISOString(),
};

describe("preview Esc close then double-click reopen", () => {
  it("returns focus to the owning pane viewport after Esc and reopens on double-click of the same entry", async () => {
    mountWorkbench();
    await settle();
    // ① 双击打开预览
    rowOf("left", rootFile.path).trigger("dblclick");
    await settle();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(true);
    // ② Esc 关闭（事件冒泡到 document 级监听，与真实键位一致）
    await wrapper!.get(".wb-preview-overlay").trigger("keydown", { key: "Escape" });
    await settle();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(false);
    // ③ 焦点确定性归还来源栏列表容器（data-pane-id 定位，不再依赖卸载时机）
    const active = document.activeElement as HTMLElement | null;
    expect(active?.classList.contains("wb-file-scroll")).toBe(true);
    expect(active?.dataset.paneId).toBe("left");
    // ④ 双击同一条目：预览重新打开（单击选中 + Enter 的既有稳定路径不回归）
    rowOf("left", rootFile.path).trigger("dblclick");
    await settle();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(true);
    expect(wrapper!.get(".wb-preview-overlay").attributes("aria-label")).toBe(rootFile.name);
  });

  it("restores focus to the right pane viewport when the preview came from the right side", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    table("right").vm.$emit("open", rootFile);
    await settle();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(true);
    await wrapper!.get(".wb-preview-overlay").trigger("keydown", { key: "Escape" });
    await settle();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(false);
    const active = document.activeElement as HTMLElement | null;
    expect(active?.classList.contains("wb-file-scroll")).toBe(true);
    expect(active?.dataset.paneId).toBe("right");
  });
});
