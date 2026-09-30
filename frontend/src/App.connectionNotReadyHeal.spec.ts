// @vitest-environment happy-dom
// issue #68（docker 安装 dbx-web 初次打开 SFTP 连接报错，必现）：sidecar 注册表
// 只在宿主 connection/connect 后有记录，而插件安装/升级/容器重启/页面刷新都会
// 造成「宿主已连接、sidecar 未注册」的窗口；0.1.83 的自愈只接在双栏切换与
// connectionId 变化的 context 事件两处。本规格钉住补上的三个入口：
// ① 面板首开（initialize）首拉失败即自愈，不再静默吞掉；
// ② 错误横幅「重试」不再裸重发——未注册类失败先请宿主补连再重放；
// ③ 宿主重开**同一**连接（connectionId 不变的 context 事件）时，栏位横幅若为
//    未注册类，借事件补一轮自愈（8s 去抖防 reopen 引发的事件成环）。
// mock 宿主不校验注册表，用 invoke/request 拦截注入真实后端的错误形状。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import { installMockHost } from "./lib/mockHost";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import { workbenchMessage } from "./lib/i18n";

let wrapper: VueWrapper | undefined;
let driver: ReturnType<typeof installMockHost> | undefined;

/** 每用例可调的宿主桥行为开关（真实宿主形状注入）。 */
const bridge = {
  /** sidecar 注册表是否有当前连接（false = 模拟 Docker 刷新后的空注册表）。 */
  registered: false,
  /** host.reopenConnection 被调用后是否真正补注册（false = 宿主 connect 失败
   * 或 ensureConnected 短路未重推 lifecycle——issue #68 截图的形态）。 */
  reopenRegisters: true,
  /** false 时 host.reopenConnection 直接拒绝（模拟宿主 connect 失败）。 */
  reopenResolves: true,
  reopenCalls: 0,
};

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
  bridge.registered = false;
  bridge.reopenRegisters = true;
  bridge.reopenResolves = true;
  bridge.reopenCalls = 0;
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0");
  driver = installMockHost();
  const bridged = window.dbxPlugin!;
  const originalInvoke = bridged.invoke.bind(bridged);
  const originalRequest = bridged.request.bind(bridged);
  Object.defineProperty(window, "dbxPlugin", {
    value: {
      ...bridged,
      invoke: (method: string, params?: Record<string, unknown>) => {
        // 注册表为空时所有指向当前连接的 files/* 一律拒绝（真实引擎文案）。
        if (bridge.registered) return originalInvoke(method, params);
        if (
          method.startsWith("files/") &&
          !method.startsWith("files/transfers") &&
          (params as { connectionId?: unknown } | undefined)?.connectionId === "mock-conn"
        ) {
          return Promise.reject(new Error("Connection is not connected (rclone engine)"));
        }
        return originalInvoke(method, params);
      },
      request: (method: string, params?: unknown) => {
        if (method === "host.reopenConnection") {
          bridge.reopenCalls += 1;
          if (!bridge.reopenResolves) return Promise.reject(new Error("Storage connect failed: check the credentials"));
          // 真实宿主 reopenConnection 的动作是向 sidecar 重推 connection/connect；
          // reopenRegisters=false 模拟宿主侧 ensureConnected 短路、未重推的形态。
          if (bridge.reopenRegisters) bridge.registered = true;
          return Promise.resolve({ ok: true });
        }
        return originalRequest(method, params);
      },
    },
    configurable: true,
  });
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  driver = undefined;
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

function banner() {
  return wrapper!.find(".wb-error-banner");
}

/** 横幅上的重试钮（模板顺序：重试在前、关闭在后）。 */
function bannerRetryButton() {
  return wrapper!.findAll(".wb-error-banner button")[0]!;
}

describe("connection-not-ready 自愈入口（issue #68）", () => {
  it("heals the first listing after mount via host.reopenConnection", async () => {
    mountWorkbench();
    await settle();

    // 首拉失败的横幅先出现；自愈（宿主补连 + 重拉）成功后横幅清空。
    expect(bridge.reopenCalls).toBe(1);
    expect(banner().exists()).toBe(false);
  });

  it("banner retry asks the host to reconnect before replaying", async () => {
    bridge.reopenResolves = false;
    mountWorkbench();
    await settle();

    // 初始自愈被宿主 connect 失败挡下，横幅滞留。
    expect(bridge.reopenCalls).toBe(1);
    expect(banner().exists()).toBe(true);

    // 宿主恢复后，用户点横幅「重试」：重放失败属未注册类 → 先补连再重放。
    bridge.reopenResolves = true;
    await bannerRetryButton().trigger("click");
    await settle();

    expect(bridge.reopenCalls).toBe(2);
    expect(banner().exists()).toBe(false);
  });

  it("re-heals on a same-connectionId context event (host re-opens the same connection)", async () => {
    // 宿主 reopen「成功」但不重推 lifecycle：ensureConnected 短路的真实形态。
    bridge.reopenRegisters = false;
    mountWorkbench();
    await settle();

    // 初始自愈跑过一轮，但注册表仍空，栏位横幅停在未注册错误上。
    expect(bridge.reopenCalls).toBe(1);
    expect(banner().exists()).toBe(true);
    expect(banner().text()).toContain(workbenchMessage("en", "errConnectionNotReady"));

    // 宿主重开同一连接：context 的 connectionId 不变，事件仍须触发补自愈。
    driver!.setContext({ connectionId: "mock-conn", connection: { name: "Mock Storage", protocol: "fs" } });
    await settle();
    expect(bridge.reopenCalls).toBe(2);
    expect(banner().exists()).toBe(true);

    // 去抖：横幅存活期（8s）内的后续同 id 事件不再重复自愈。
    driver!.setContext({ connectionId: "mock-conn", connection: { name: "Mock Storage", protocol: "fs" } });
    await settle();
    expect(bridge.reopenCalls).toBe(2);

    // 去抖窗口过后、宿主真正补上注册：事件驱动的自愈收尾成功。
    await vi.advanceTimersByTimeAsync(8_000);
    bridge.reopenRegisters = true;
    driver!.setContext({ connectionId: "mock-conn", connection: { name: "Mock Storage", protocol: "fs" } });
    await settle();
    expect(bridge.reopenCalls).toBe(3);
    expect(banner().exists()).toBe(false);
  });

  it("does not disturb a healthy panel on same-connectionId context events", async () => {
    bridge.registered = true;
    mountWorkbench();
    await settle();
    expect(banner().exists()).toBe(false);

    driver!.setContext({ connectionId: "mock-conn", connection: { name: "Mock Storage", protocol: "fs" } });
    await settle();
    expect(bridge.reopenCalls).toBe(0);
    expect(banner().exists()).toBe(false);
  });
});
