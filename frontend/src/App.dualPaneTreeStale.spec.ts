// @vitest-environment happy-dom
// 双栏开关的左栏数据源切换（单栏=宿主当前连接 ↔ 双栏=__local__ 本地）必须与
// 连接切换同法整树重建：否则 tree tab 继续展示上一数据源的缓存子树（连接
// bucket namespace 存储后开双栏，左栏主列表已是本地文件、左侧树仍是远端桶）。
// 同时路径栏根 crumb（bucketRootLabel）按栏取值：本地栏不因宿主当前连接是
// namespace 连接而显示「桶」。mock 宿主经 invoke 拦截注入 bucketNamespace 能力。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileToolbar from "./components/FileToolbar.vue";
import SideNavPanel from "./components/SideNavPanel.vue";
import PathField from "./components/PathField.vue";
import { installMockHost } from "./lib/mockHost";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import { workbenchMessage } from "./lib/i18n";

let wrapper: VueWrapper | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  saveUiPrefs({
    sort: { column: "name", direction: "asc" },
    leftSideTab: "tree",
    rightSideTab: "tree",
    leftSideCollapsed: false,
    rightSideCollapsed: false,
  });
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0");
  installMockHost();
  // 宿主当前连接按 bucket namespace 能力上报（S3 类连接形状：根目录即桶列表）。
  const bridged = window.dbxPlugin!;
  const originalInvoke = bridged.invoke.bind(bridged);
  Object.defineProperty(window, "dbxPlugin", {
    value: {
      ...bridged,
      invoke: async (method: string, params?: unknown, options?: { timeoutMs?: number }) => {
        const result = await originalInvoke(method, params, options);
        if (method === "files/capabilities") return { ...(result as Record<string, unknown>), bucketNamespace: true };
        return result;
      },
    },
    configurable: true,
  });
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

async function toggleDualPane() {
  const button = wrapper!.getComponent(FileToolbar).findAll("button")
    .find((button) => button.attributes("aria-label") === workbenchMessage("en", "dualPane"))!;
  await button.trigger("click");
  await settle();
}

function leftPanel() {
  return wrapper!.findAllComponents(SideNavPanel).find((panel) => panel.props("side") === "left")!;
}

function treeRow(path: string) {
  // v-tip 迁移：悬浮提示文案同步写在 aria-label（webview 不渲染原生 title）。
  return leftPanel().findAll(".wb-tree-row").find((row) => row.attributes("aria-label") === path);
}

function panePathFields() {
  return wrapper!.findAllComponents(PathField);
}

function leftFirstCrumb() {
  // 面包屑在左栏 pane header（wb-pane-source）里，不在 SideNavPanel 内。
  return wrapper!.find(".wb-pane-source .wb-breadcrumbs button");
}

describe("dual-pane toggle rebuilds the left pane tree for its data source", () => {
  it("drops the host-connection tree and shows the local root when dual pane turns on", async () => {
    mountWorkbench();
    await settle();
    // 单栏左栏即宿主当前连接：树已加载 mock-conn 根子树（复现前提）。
    expect(treeRow("/pictures")).toBeDefined();

    await toggleDualPane();

    // 左栏已切到 __local__ 本地：旧连接缓存子树必须整树重建让位，
    // 树根重拉为本地根（/Applications、/tmp），并跟随定位本地当前目录。
    expect(treeRow("/pictures")).toBeUndefined();
    expect(treeRow("/Applications")).toBeDefined();
    expect(treeRow("/tmp")).toBeDefined();
  });

  it("restores the host-connection tree when dual pane turns off", async () => {
    mountWorkbench();
    await settle();
    await toggleDualPane();
    expect(treeRow("/pictures")).toBeUndefined();

    await toggleDualPane();

    // 关闭双栏：左栏回到宿主当前连接，树重建后重新展示该连接的根子树。
    expect(treeRow("/pictures")).toBeDefined();
    expect(treeRow("/Applications")).toBeUndefined();
  });

  it("keeps the bucket root crumb off the local pane path bar only", async () => {
    mountWorkbench();
    await settle();
    // 单栏=namespace 连接：根 crumb 显示「桶」（既有行为）。
    expect(panePathFields()[0]!.props("rootLabel")).toBe(workbenchMessage("en", "bucketRootLabel"));

    await toggleDualPane();

    // 左栏=本地：根 crumb 回落 "/"；右栏保持 namespace 连接语义不变。
    expect(panePathFields()[0]!.props("rootLabel")).toBe("");
    expect(panePathFields()[1]!.props("rootLabel")).toBe(workbenchMessage("en", "bucketRootLabel"));
    expect(leftFirstCrumb()!.text()).toBe("/");

    await toggleDualPane();

    // 关闭双栏：左栏回到 namespace 连接，根 crumb 恢复「桶」。
    expect(panePathFields()[0]!.props("rootLabel")).toBe(workbenchMessage("en", "bucketRootLabel"));
  });
});
