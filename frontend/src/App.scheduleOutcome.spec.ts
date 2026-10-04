// @vitest-environment happy-dom
// 计划任务终态可见性（0.1.89 review）：failed/skipped 的 files/schedule/run
// 事件必须升全局错误条——只落 dock 页签等于静默失败，对备份功能是最危险
// 的形态。同一任务的连续同状态失败只提醒一次（成功清档，新失败再提醒）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import App from "./App.vue";
import { installMockHost } from "./lib/mockHost";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";

let wrapper: VueWrapper | undefined;
let driver: ReturnType<typeof installMockHost> | undefined;

function errorBanner(): string | null {
  const banner = wrapper!.find(".wb-error-banner");
  return banner.exists() ? banner.text() : null;
}

function emitRun(run: Record<string, unknown>): void {
  driver!.emitEvent("files/schedule/run", { run: { trigger: "schedule", bytes: 0, ...run } });
}

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
  driver = installMockHost();
  wrapper = mount(App, { attachTo: document.body, global: { directives: { tip: vTip } } });
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  vi.useRealTimers();
});

describe("schedule run terminal visibility", () => {
  it("raises failed and skipped runs to the global error bar with the task name", async () => {
    await vi.advanceTimersByTimeAsync(300);
    emitRun({ runId: "r1", taskId: "t-backup", status: "failed", error: "quota exceeded", startedAt: 1 });
    await vi.advanceTimersByTimeAsync(50);
    expect(errorBanner()).toContain("t-backup");
    expect(errorBanner()).toContain("quota exceeded");

    emitRun({ runId: "r2", taskId: "t-offline", status: "skipped", error: "connection unavailable: boom", startedAt: 2 });
    await vi.advanceTimersByTimeAsync(50);
    expect(errorBanner()).toContain("t-offline");
    expect(errorBanner()).toContain("skipped");
  });

  it("keeps a repeated same-status failure quiet until a success breaks the streak", async () => {
    await vi.advanceTimersByTimeAsync(300);
    emitRun({ runId: "r1", taskId: "t1", status: "failed", error: "boom", startedAt: 1 });
    await vi.advanceTimersByTimeAsync(50);
    expect(errorBanner()).toContain("boom");
    // 连续失败（分钟级 cron 断连场景）不再反复弹横幅——横幅保持但内容不
    // 被新失败重写（以 runId 计的事件不产生新提醒）。
    const firstBanner = errorBanner();
    emitRun({ runId: "r2", taskId: "t1", status: "failed", error: "boom again", startedAt: 2 });
    await vi.advanceTimersByTimeAsync(50);
    expect(errorBanner()).toBe(firstBanner);

    // 成功清档：下一次失败重新提醒。
    emitRun({ runId: "r3", taskId: "t1", status: "success", bytes: 10, startedAt: 3 });
    await vi.advanceTimersByTimeAsync(50);
    emitRun({ runId: "r4", taskId: "t1", status: "failed", error: "fresh boom", startedAt: 4 });
    await vi.advanceTimersByTimeAsync(50);
    expect(errorBanner()).toContain("fresh boom");
  });
});
