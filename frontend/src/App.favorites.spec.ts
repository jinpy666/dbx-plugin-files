// @vitest-environment happy-dom
// 收藏夹（rclone-ui parity）UI 冒烟：工具栏星标收藏/取消收藏活动栏当前目录并
// 落 localStorage（按连接键入）；侧栏 fav tab 列表点击导航；侧栏行右键
// 「收藏 / 从收藏移除」切换；七语 key 完整性。真实 App + mock 桥。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import PathField from "./components/PathField.vue";
import { installMockHost } from "./lib/mockHost";
import { messages, workbenchMessage, type WorkbenchLocale } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { FAVORITES_KEY, prefsStore, saveUiPrefs, UI_PREFS_KEY } from "./lib/prefs";

let wrapper: VueWrapper | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  // 默认持久化后端是 prefsStore（宿主 storage 适配），播种/清理须走同一实例。
  prefsStore.removeItem(UI_PREFS_KEY);
  prefsStore.removeItem(FAVORITES_KEY);
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

function storedFavorites(): Record<string, string[]> {
  const raw = prefsStore.getItem(FAVORITES_KEY);
  return raw ? (JSON.parse(raw) as Record<string, string[]>) : {};
}

function starButton() {
  return wrapper!
    .getComponent(FileToolbar)
    .findAll("button")
    .find((button) => [workbenchMessage("en", "favAdd"), workbenchMessage("en", "favRemove")].includes(button.attributes("aria-label") ?? ""))!;
}

function sideTabButton(label: string) {
  return wrapper!
    .findAll(".wb-side-tabs button")
    .find((button) => button.attributes("aria-label") === label)!;
}

function paneTable(paneId: "left" | "right") {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === paneId)!;
}

async function openDualPane() {
  const button = wrapper!.getComponent(FileToolbar).findAll("button")
    .find((button) => button.attributes("aria-label") === workbenchMessage("en", "dualPane"))!;
  await button.trigger("click");
  await settle();
}

describe("toolbar favorite star (rclone-ui parity 收藏当前目录)", () => {
  it("stars the current directory and persists it under the connection id", async () => {
    mountWorkbench();
    await settle();
    const star = starButton();
    expect(star.attributes("aria-label")).toBe(workbenchMessage("en", "favAdd"));
    expect(star.attributes("aria-pressed")).toBe("false");
    await star.trigger("click");
    await settle();
    // 单栏左栏即当前连接 mock-conn，当前目录 "/"。
    expect(storedFavorites()).toEqual({ "mock-conn": ["/"] });
    expect(starButton().attributes("aria-pressed")).toBe("true");
    // 再点取消：连接键从收藏表中移除。
    await starButton().trigger("click");
    await settle();
    expect(storedFavorites()).toEqual({});
  });

  it("keys favorites per connection across the dual-pane local/remote sides", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    // 双栏左栏起点可能是本机主目录（enterLocalPaneIfAtRoot），动态取当前路径。
    const leftPath = () => wrapper!.findAllComponents(PathField)[0].props("path") as string;
    const rightPath = () => wrapper!.findAllComponents(PathField)[1].props("path") as string;
    // 左栏（__local__ 本地面）当前目录收藏。
    await starButton().trigger("click");
    await settle();
    expect(storedFavorites()).toEqual({ __local__: [leftPath()] });
    // 激活右栏（mock-conn）后收藏当前目录：按连接分键，互不覆盖。
    paneTable("right").vm.$emit("update:selection", ["/docs"]);
    await settle();
    await starButton().trigger("click");
    await settle();
    expect(storedFavorites()).toEqual({ __local__: [leftPath()], "mock-conn": [rightPath()] });
    // 取消右栏收藏：__local__ 键不受影响。
    await starButton().trigger("click");
    await settle();
    expect(storedFavorites()).toEqual({ __local__: [leftPath()] });
  });
});

describe("favorites side tab", () => {
  it("lists starred paths and navigates the pane on click", async () => {
    mountWorkbench();
    await settle();
    // 收藏当前目录 "/"，随后导航到 /docs 再从 fav tab 点回 "/"。
    await starButton().trigger("click");
    await settle();
    const pathField = wrapper!.getComponent(PathField);
    pathField.vm.$emit("navigate", "/docs");
    await settle();
    expect(pathField.props("path")).toBe("/docs");
    await sideTabButton(workbenchMessage("en", "favTitle")).trigger("click");
    await settle();
    const rows = wrapper!.findAll(".wb-fav-row");
    expect(rows).toHaveLength(1);
    expect(rows[0].attributes("title")).toBe("/");
    await rows[0].trigger("click");
    await settle();
    expect(wrapper!.getComponent(PathField).props("path")).toBe("/");
  });

  it("shows the empty hint when nothing is starred", async () => {
    mountWorkbench();
    await settle();
    await sideTabButton(workbenchMessage("en", "favTitle")).trigger("click");
    await settle();
    expect(wrapper!.find(".wb-side-empty").text()).toBe(workbenchMessage("en", "favEmpty"));
  });
});

describe("sidebar row context menu favorite toggle", () => {
  it("removes a favorite from the fav tab rows and adds it back from a tree row", async () => {
    mountWorkbench();
    await settle();
    await starButton().trigger("click");
    await settle();
    // fav 行右键 → 已收藏态显示「从收藏移除」。
    await sideTabButton(workbenchMessage("en", "favTitle")).trigger("click");
    await settle();
    await wrapper!.findAll(".wb-fav-row")[0].trigger("contextmenu", { clientX: 10, clientY: 10 });
    await settle();
    const removeItem = wrapper!.findAll("[role=menuitem]").find((item) => item.text() === workbenchMessage("en", "favRemove"))!;
    await removeItem.trigger("click");
    await settle();
    expect(storedFavorites()).toEqual({});
    // 目录树根行右键 → 未收藏态显示「收藏」。
    await sideTabButton(workbenchMessage("en", "sideTree")).trigger("click");
    await settle();
    const rootRow = wrapper!.findAll(".wb-tree-row")[0];
    await rootRow.trigger("contextmenu", { clientX: 10, clientY: 10 });
    await settle();
    const addItem = wrapper!.findAll("[role=menuitem]").find((item) => item.text() === workbenchMessage("en", "favAdd"))!;
    await addItem.trigger("click");
    await settle();
    expect(storedFavorites()).toEqual({ "mock-conn": ["/"] });
  });
});

// i18n 完整性：本批次新增 key 在七语中全部落位（i18n.spec 已校验全量对齐，
// 这里针对新 key 再做一次显式断言，防止单条遗漏）。
describe("favorites/drop-action i18n keys", () => {
  const NEW_KEYS = ["favTitle", "favEmpty", "favAdd", "favRemove", "dropActionTitle", "dropActionTarget", "dropActionItems", "dropActionCopy", "dropActionMove"];

  it("resolve to real copy in all seven locales", () => {
    const locales = Object.keys(messages) as WorkbenchLocale[];
    expect(locales).toHaveLength(7);
    for (const key of NEW_KEYS) {
      for (const locale of locales) {
        expect(workbenchMessage(locale, key), `${locale}/${key}`).not.toBe(key);
        expect(workbenchMessage(locale, key).length).toBeGreaterThan(0);
      }
    }
    expect(workbenchMessage("zh-CN", "favAdd")).toBe("收藏");
    expect(workbenchMessage("zh-TW", "favRemove")).toBe("從收藏移除");
    expect(workbenchMessage("en", "dropActionItems", { count: 3 })).toContain("3");
  });
});
