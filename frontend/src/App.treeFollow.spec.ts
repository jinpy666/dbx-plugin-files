// @vitest-environment happy-dom
// issue #66 侧栏目录树跟随定位 UI 冒烟：tree tab 可见时
// ① 挂载后根目录自动展开（无需点击）；
// ② 本栏导航进入嵌套目录后，树沿路径链自动展开祖先并定位（is-current 高亮）；
// ③ 从 quick tab 切回 tree tab 时根展开并定位当前目录。
// 真实 App + FileTable + SideNavPanel + mock 桥，不触达真实文件或连接。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import SideNavPanel from "./components/SideNavPanel.vue";
import PathField from "./components/PathField.vue";
import { installMockHost } from "./lib/mockHost";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import type { FileEntry } from "./lib/api";

let wrapper: VueWrapper | undefined;

function seedPrefs(leftSideTab: "tree" | "quick") {
  saveUiPrefs({
    sort: { column: "name", direction: "asc" },
    leftSideTab,
    rightSideTab: "tree",
    leftSideCollapsed: false,
    rightSideCollapsed: false,
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  seedPrefs("tree");
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

/** 单栏模式左栏即当前连接（mock-conn）。 */
async function openDirectory(entry: FileEntry) {
  table().vm.$emit("open", entry);
  await settle();
}

function leftPanel() {
  return wrapper!.findAllComponents(SideNavPanel).find((panel) => panel.props("side") === "left")!;
}

function PathFieldComponent() {
  return wrapper!.getComponent(PathField);
}

function treeRow(path: string) {
  return leftPanel().findAll(".wb-tree-row").find((row) => row.attributes("title") === path);
}

describe("side tree follows the pane directory (issue #66)", () => {
  it("expands the tree root on mount while the tree tab is visible", async () => {
    mountWorkbench();
    await settle();
    // 根行 + 至少一层子目录行：无需任何点击即自动展开根目录。
    expect(leftPanel().findAll(".wb-tree-row").length).toBeGreaterThan(1);
  });

  it("expands the ancestor chain and marks the nested directory as current after pane navigation", async () => {
    mountWorkbench();
    await settle();
    const pictures = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures")!;
    await openDirectory(pictures);
    const nested = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures/2024")!;
    await openDirectory(nested);
    // 本栏进入嵌套目录后，树自动展开 /pictures 并定位 /pictures/2024。
    const row = treeRow("/pictures/2024");
    expect(row).toBeDefined();
    expect(row!.classes()).toContain("is-current");
  });

  it("keeps the chain expanded and the current mark after a sidebar tree refresh", async () => {
    mountWorkbench();
    await settle();
    const pictures = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures")!;
    await openDirectory(pictures);
    const nested = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures/2024")!;
    await openDirectory(nested);
    // tree tab 工具条刷新钮（tabs 顺序：tree/quick/fav/刷新/收起）。
    await leftPanel().findAll(".wb-side-tabs button")[3]!.trigger("click");
    await settle();
    // 刷新重拉根后恢复定位：链路保持展开，is-current 仍在当前目录。
    expect(leftPanel().findAll(".wb-tree-row").length).toBeGreaterThan(1);
    const row = treeRow("/pictures/2024");
    expect(row).toBeDefined();
    expect(row!.classes()).toContain("is-current");
  });

  it("keeps manually expanded unrelated nodes when following a new directory", async () => {
    mountWorkbench();
    await settle();
    // 手动展开跟目标链无关的 /media（含子目录 /media/clips）。
    await treeRow("/media")!.find(".wb-tree-caret").trigger("click");
    await settle();
    expect(treeRow("/media/clips")).toBeDefined();
    const pictures = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures")!;
    await openDirectory(pictures);
    const nested = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures/2024")!;
    await openDirectory(nested);
    // 跟随只沿路径链展开（只置 expanded，不折叠任何节点）：/media 保持展开。
    expect(treeRow("/media/clips")).toBeDefined();
    const row = treeRow("/pictures/2024");
    expect(row).toBeDefined();
    expect(row!.classes()).toContain("is-current");
  });

  it("stays error-free and relocates when navigating after a tree refresh", async () => {
    mountWorkbench();
    await settle();
    const pictures = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures")!;
    await openDirectory(pictures);
    // 侧栏刷新：整树重建 + 在途跟随链作废（pane 停在 /pictures 不受影响）。
    await leftPanel().findAll(".wb-side-tabs button")[3]!.trigger("click");
    await settle();
    // 回根后改航 /media：新跟随链定位新目录、无错误横幅，且先前展开的
    // /pictures 链不被折叠（只展开策略）。
    PathFieldComponent().vm.$emit("navigate", "/");
    await settle();
    const media = table().props("entries").find((entry: FileEntry) => entry.path === "/media")!;
    await openDirectory(media);
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
    const row = treeRow("/media");
    expect(row).toBeDefined();
    expect(row!.classes()).toContain("is-current");
    expect(treeRow("/pictures/2024")).toBeDefined();
  });

  it("expands and locates the current directory when switching back to the tree tab", async () => {
    seedPrefs("quick");
    mountWorkbench();
    await settle();
    const pictures = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures")!;
    await openDirectory(pictures);
    const nested = table().props("entries").find((entry: FileEntry) => entry.path === "/pictures/2024")!;
    await openDirectory(nested);
    // quick tab 下树未展开：嵌套目录行不存在。
    expect(treeRow("/pictures/2024")).toBeUndefined();
    // 切到 tree tab：根展开并直接定位当前目录。
    await leftPanel().findAll(".wb-side-tabs button")[0]!.trigger("click");
    await settle();
    expect(leftPanel().findAll(".wb-tree-row").length).toBeGreaterThan(1);
    const row = treeRow("/pictures/2024");
    expect(row).toBeDefined();
    expect(row!.classes()).toContain("is-current");
  });
});
