// @vitest-environment happy-dom
// 双栏跨连接切换（HOST_FEEDBACK F-5）：目标连接未在 sidecar registry 注册时
// （真实宿主只为激活连接调用 connection/connect），切换后的首个 files/* 调用
// 报 "Connection is not connected (rclone engine)"。App 侧契约：
// ① 先请宿主跑 connect（host.reopenConnection，凭据在宿主侧）后重试一次；
// ② 宿主 connect 失败/旧宿主缺方法时回退原连接并给出可操作文案，栏位不滞留。
// mock 宿主不校验 registry，用 invoke/request 拦截注入真实后端的错误形状。
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

/** 每用例可调的宿主桥行为开关（真实宿主形状注入）。 */
const bridge = {
  /** host.reopenConnection 对 bad-conn 拒绝（模拟宿主 connect 失败）。 */
  reopenRejects: true,
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
  bridge.reopenRejects = true;
  bridge.reopenCalls = 0;
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0");
  installMockHost();
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
        if (method === "host.reopenConnection" && (params as { connectionId?: string } | undefined)?.connectionId === BAD_CONNECTION_ID) {
          bridge.reopenCalls += 1;
          if (bridge.reopenRejects) return Promise.reject(new Error("Storage connect failed: check the credentials"));
          // 真实宿主 reopenConnection 的实际动作：向 sidecar 发
          // connection/connect 注册该连接——mock 注册后 files/* 即放行。
          return originalInvoke("connection/connect", {
            connection: { id: BAD_CONNECTION_ID, name: "Unregistered Storage", external_config: { protocol: "fs" } },
          }).then(() => ({ ok: true }));
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

function targetConnectionTrigger() {
  return wrapper!.find(`button[aria-label="${workbenchMessage("en", "targetConnection")}"]`);
}

/** 打开右栏连接下拉并点选指定选项（ConnectionSelect 自定义弹层）。 */
async function pickTargetConnection(name: string) {
  await targetConnectionTrigger().trigger("click");
  await nextTick();
  const option = wrapper!.findAll(".wb-conn-select-menu [role='option']")
    .find((option) => option.text().includes(name))!;
  await option.trigger("click");
  await settle();
}

describe("dual-pane cross-connection switch (F-5 reopen 自愈与回退)", () => {
  it("recovers via host.reopenConnection when the target connection registers", async () => {
    // 未注册 → 首拉失败 → 宿主 connect 成功（清除拒绝）→ 重试放行。
    bridge.reopenRejects = false;
    mountWorkbench();
    await settle();
    await openDualPane();

    expect(targetConnectionTrigger().exists()).toBe(true);
    await pickTargetConnection("Unregistered Storage");

    // 自愈契约：宿主 connect 被请求一次，重试拉取成功，栏位停在新连接、无横幅。
    expect(bridge.reopenCalls).toBe(1);
    expect(targetConnectionTrigger().attributes("title")).toBe("Unregistered Storage");
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
  });

  it("reverts to the previous connection when the host connect fails", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();

    await pickTargetConnection("Unregistered Storage");

    // 回退契约：宿主 connect 失败 → 选择器回原连接，横幅给出可操作指引而非原始引擎文案。
    expect(bridge.reopenCalls).toBe(1);
    expect(targetConnectionTrigger().attributes("title")).toBe(workbenchMessage("en", "sameConnection"));
    const banner = wrapper!.find(".wb-error-banner");
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain(workbenchMessage("en", "errConnectionNotReady"));
    expect(banner.text()).not.toContain("Unknown connectionId");
  });
});
