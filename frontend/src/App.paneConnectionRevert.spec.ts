// @vitest-environment happy-dom
// 双栏跨连接切换回退（HOST_FEEDBACK F-5）：目标连接未在 sidecar registry
// 注册时（真实宿主只为激活连接调用 connection/connect），切换后的首个
// files/* 调用报 "Connection is not connected (rclone engine)"。App 侧契约：
// ① 该错误类映射为可操作的友好文案；② 该栏自动回退到原连接并恢复列表，
// 不滞留在不可用连接上。mock 宿主不校验 registry，这里用 invoke 拦截注入
// 真实后端的错误形状。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileToolbar from "./components/FileToolbar.vue";
import { installMockHost } from "./lib/mockHost";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import { workbenchMessage } from "./lib/i18n";

let wrapper: VueWrapper | undefined;

const BAD_CONNECTION_ID = "bad-conn";
const NOT_CONNECTED = "Connection is not connected (rclone engine)";

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  saveUiPrefs({
    sort: { column: "name", direction: "asc" },
    leftSideTab: "quick",
    rightSideTab: "quick",
    leftSideCollapsed: false,
    rightSideCollapsed: false,
  });
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0");
  installMockHost();
  // 真实宿主形状注入：listConnections 多给一个未注册连接；该连接的 files/*
  // 一律按 binding() 的 registry miss 报错。
  const bridged = window.dbxPlugin!;
  const originalInvoke = bridged.invoke.bind(bridged);
  const originalRequest = bridged.request.bind(bridged);
  Object.defineProperty(window, "dbxPlugin", {
    value: {
      ...bridged,
      request: (method: string, params?: unknown) => {
        if (method === "host.listConnections") {
          return Promise.resolve([
            { id: "mock-conn", name: "Mock Storage" },
            { id: BAD_CONNECTION_ID, name: "Unregistered Storage" },
          ]);
        }
        return originalRequest(method, params);
      },
      invoke: (method: string, params?: Record<string, unknown>, options?: { timeoutMs?: number }) => {
        if (method.startsWith("files/") && (params as { connectionId?: string } | undefined)?.connectionId === BAD_CONNECTION_ID) {
          return Promise.reject(new Error(NOT_CONNECTED));
        }
        return originalInvoke(method, params, options);
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

async function openDualPane() {
  const button = wrapper!.getComponent(FileToolbar).findAll("button")
    .find((button) => button.attributes("aria-label") === workbenchMessage("en", "dualPane"))!;
  await button.trigger("click");
  await settle();
}

function targetConnectionSelect() {
  return wrapper!.find(`select[aria-label="${workbenchMessage("en", "targetConnection")}"]`);
}

describe("dual-pane cross-connection switch revert (registry-miss 回退)", () => {
  it("reverts the right pane to the previous connection and shows actionable guidance", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();

    const select = targetConnectionSelect();
    expect(select.exists()).toBe(true);
    // 初始为「同连接」（value=""），右栏展示 mock-conn 的根目录。
    expect((select.element as HTMLSelectElement).value).toBe("");

    await select.setValue(BAD_CONNECTION_ID);
    await settle();

    // 回退契约：选择器回到原连接，错误横幅给出可操作指引而非原始引擎文案。
    expect((select.element as HTMLSelectElement).value).toBe("");
    const banner = wrapper!.find(".wb-error-banner");
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain(workbenchMessage("en", "errConnectionNotReady"));
    expect(banner.text()).not.toContain(NOT_CONNECTED);
  });
});
