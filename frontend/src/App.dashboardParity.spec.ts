// @vitest-environment happy-dom
// 对标 yet-another-rclone-dashboard 的特性补齐 UI smoke：
// ① 目录「计算大小」（files/size）②「复制公开链接」（files/publicLink，
// capabilities.presign 门控）③ 预览浮窗最小化 pill。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { defineComponent, nextTick, ref, type Component } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import type { FileEntry } from "./lib/api";

// 可编程 PreviewPane 桩：真实组件由 PreviewPane.spec 覆盖，这里驱动
// App 层的 minimize 事件接线（桩面板 → is-minimized → pill 还原）。
const PreviewPaneStub = defineComponent({
  props: { path: { type: String, default: null } },
  emits: ["close", "saved", "download", "minimize"],
  setup(_props, { expose }) {
    const isDirty = ref(false);
    expose({ isDirty });
    return { isDirty };
  },
  template: `<div class="stub-preview"><button data-test="stub-min" @click="$emit('minimize')">min</button><button data-test="stub-close" @click="$emit('close')">close</button></div>`,
});

let wrapper: VueWrapper | undefined;

function installHost(search: string) {
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", search);
  return installMockHost()!;
}

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
  installHost("/?mock=1&locale=en&delay=0");
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "dbxPlugin");
});

async function settle() {
  for (let i = 0; i < 8; i++) {
    await vi.advanceTimersByTimeAsync(1);
    await nextTick();
  }
}

function mountWorkbench(stubs?: Record<string, boolean | Component>) {
  wrapper = mount(App, { attachTo: document.body, global: { directives: { tip: vTip }, stubs } });
  return wrapper;
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

describe("dashboard parity: directory size & public link", () => {
  it("「Calculate size」on a directory reports files/size totals in the notice", async () => {
    mountWorkbench();
    await settle();
    await openEntryMenu({ name: "docs", path: "/docs", kind: "directory", size: 0, modifiedAt: new Date().toISOString() });
    await menuItem(workbenchMessage("en", "computeSize"))!.trigger("click");
    await settle();
    // mock 树 /docs 下 13 个直接文件；notice 汇总 count · bytes。
    const notice = wrapper!.get(".wb-notice");
    expect(notice.text()).toContain("13 items · ");
    wrapper!.unmount();
    wrapper = undefined;
  });

  it("「Copy public link」is hidden when the backend reports no presign capability", async () => {
    mountWorkbench();
    await settle();
    await openEntryMenu({ name: "readme.md", path: "/docs/readme.md", kind: "file", size: 10, modifiedAt: new Date().toISOString() });
    expect(menuItem(workbenchMessage("en", "copyPublicLink"))).toBeUndefined();
    wrapper!.unmount();
    wrapper = undefined;
  });

  it("「Copy public link」writes the presigned URL to the clipboard when presign is supported", async () => {
    installHost("/?mock=1&locale=en&delay=0&presign=1");
    const writeText = vi.fn(async () => undefined);
    const clipboard = window.dbxPlugin!.clipboard as unknown as { writeText: (value: string) => Promise<void> };
    clipboard.writeText = writeText;
    mountWorkbench();
    await settle();
    await openEntryMenu({ name: "readme.md", path: "/docs/readme.md", kind: "file", size: 10, modifiedAt: new Date().toISOString() });
    await menuItem(workbenchMessage("en", "copyPublicLink"))!.trigger("click");
    await settle();
    expect(writeText).toHaveBeenCalledWith("https://mock.example/presigned/docs/readme.md?X-Expires=audit");
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("en", "copiedPublicLink"));
    wrapper!.unmount();
    wrapper = undefined;
  });
});

describe("dashboard parity: preview minimize pill", () => {
  it("minimize hides the panel into a floating pill; clicking the pill restores it", async () => {
    mountWorkbench({ PreviewPane: PreviewPaneStub });
    await settle();
    table().vm.$emit("open", { name: "readme.md", path: "/docs/readme.md", kind: "file", size: 10, modifiedAt: new Date().toISOString() } satisfies FileEntry);
    await settle();
    const overlay = wrapper!.get(".wb-preview-overlay");
    expect(overlay.classes()).not.toContain("is-minimized");

    await wrapper!.get("[data-test=stub-min]").trigger("click");
    await nextTick();
    expect(wrapper!.get(".wb-preview-overlay").classes()).toContain("is-minimized");
    const pill = wrapper!.get(".wb-preview-pill");
    expect(pill.text()).toBe("readme.md");

    await pill.trigger("click");
    await nextTick();
    expect(wrapper!.get(".wb-preview-overlay").classes()).not.toContain("is-minimized");
    expect(wrapper!.find(".wb-preview-pill").exists()).toBe(false);
  });
});
