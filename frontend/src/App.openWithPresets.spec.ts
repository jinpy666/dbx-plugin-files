// @vitest-environment happy-dom
// 平台「打开方式」预设走查：?local=1&platform=macos 模拟桌面宿主后，
// 设置弹窗 openWith 分类展示 files/local/detect-apps 探测到的本机应用
// （WPS/Excel/...），点击预设 → validate-open-app → settingsSaved 反馈。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";

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
  // local=1 必须在 installMockHost 前生效（mock 在 install 时读 query）。
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1&platform=macos");
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

describe("open-with platform presets", () => {
  it("lists detected apps and applies a preset with save feedback", async () => {
    mountWorkbench();
    await settle();
    const settingsButton = wrapper!
      .get(".wb-toolbar-actions")
      .findAll("button")
      .find((button) => button.attributes("aria-label") === workbenchMessage("en", "settings"))!;
    await settingsButton.trigger("click");
    await settle();
    // 切到 openWith 分类：预设区渲染 detect-apps 的 macos 候选。
    await wrapper!
      .get(".wb-settings-nav")
      .findAll("button")
      .find((button) => button.text() === workbenchMessage("en", "settingsNav.openWith"))!
      .trigger("click");
    await settle();
    const presets = wrapper!.findAll(".wb-preset-chip");
    const names = presets.map((chip) => chip.text());
    expect(names).toContain("WPS Office");
    expect(names).toContain("Microsoft Excel");
    // 点击预设：只填草稿（统一保存模型），此时尚不发起 validate。
    const invoke = vi.spyOn(window.dbxPlugin, "invoke");
    const wps = presets.find((chip) => chip.text() === "WPS Office")!;
    await wps.trigger("click");
    await settle();
    expect(invoke).not.toHaveBeenCalledWith(
      "files/local/validate-open-app",
      { path: "/Applications/wpsoffice.app" },
    );
    // 弹窗底部「保存更改」→ validate-open-app 校验 → 持久化 + settingsSaved 反馈。
    await wrapper!.get(".wb-settings-save").trigger("click");
    await settle();
    expect(invoke).toHaveBeenCalledWith(
      "files/local/validate-open-app",
      { path: "/Applications/wpsoffice.app" },
    );
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("en", "settingsSaved"));
    // 默认应用输入框同步为预设路径。
    const appInput = wrapper!.findAll(".wb-settings-path-row input")[0]!;
    expect((appInput.element as HTMLInputElement).value).toBe("/Applications/wpsoffice.app");
  });
});
