// @vitest-environment happy-dom
// MCP UI intent 通道（M2）验收：shared/frontend/uiIntent 公共层在本插件工具
// 链下的行为（事件归一化）+ App.vue 真实接线走查——mock 桥按 sidecar
// `files/ui/intent` 形状注入事件，断言落 UI（导航/面板切换/行定位）与
// `files/ui/state/report` 回报（applied/rejected + 快照型）。
// mockHost 的新事件/方法形状同步镜像（防单测脱节，AGENTS.md 硬性规则 7）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "../App.vue";
import FileTable from "../components/FileTable.vue";
import { emitUiIntent, installMockHost } from "./mockHost";
import { workbenchMessage } from "./i18n";
import { readUiIntentEvent } from "../../../shared/frontend/uiIntent";

let wrapper: VueWrapper | undefined;
let invoke: ReturnType<typeof vi.spyOn>;

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  Reflect.deleteProperty(window, "dbxPlugin");
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0");
  installMockHost();
  invoke = vi.spyOn(window.dbxPlugin, "invoke");
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
  wrapper = mount(App, { attachTo: document.body });
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

type ReportBody = { intentId?: string; status: string; summary?: Record<string, unknown> };
const reportBodies = (): ReportBody[] =>
  invoke.mock.calls
    .filter(([method]: [string, unknown]) => method === "files/ui/state/report")
    .map(([, params]: [string, unknown]) => params as ReportBody);

const intentReport = (intentId: string) => reportBodies().find((body) => body.intentId === intentId);

describe("readUiIntentEvent normalization (shared layer)", () => {
  it("normalizes files/ui/intent and ignores unrelated events", () => {
    expect(readUiIntentEvent({ type: "env", locale: "en" }, "files")).toBeNull();
    expect(readUiIntentEvent({ method: "files/transfer/progress", params: {} }, "files")).toBeNull();
    expect(readUiIntentEvent({ method: "ldap/ui/intent", params: { intentId: "x", action: "search" } }, "files")).toBeNull();
    expect(
      readUiIntentEvent({ method: "files/ui/intent", params: { intentId: "i-1", action: "search" } }, "files"),
    ).toEqual({ intentId: "i-1", action: "search", params: {} });
    expect(
      readUiIntentEvent({ method: "files/ui/intent", params: { intentId: "i-2", action: "focus", params: { panel: "audit" } } }, "files"),
    ).toEqual({ intentId: "i-2", action: "focus", params: { panel: "audit" } });
    // intentId/action 缺失或非字符串一律拒绝（防宿主形状漂移静默通过）。
    expect(readUiIntentEvent({ method: "files/ui/intent", params: { action: "search" } }, "files")).toBeNull();
    expect(readUiIntentEvent({ method: "files/ui/intent" }, "files")).toBeNull();
    expect(readUiIntentEvent(null, "files")).toBeNull();
  });
});

describe("workbench intent wiring (App.vue + mock host)", () => {
  it("fills the path, triggers the existing navigation pipeline and reports applied with a listing summary", async () => {
    mountWorkbench();
    await settle();
    invoke.mockClear();
    emitUiIntent({ intentId: "i-nav", action: "search", params: { path: "/docs" } });
    await settle();
    // 导航走既有 files/list 管线（PathField 随 path 变化同步展示）。
    const listings = invoke.mock.calls.filter(
      ([method, params]: [string, unknown]) => method === "files/list" && (params as Record<string, unknown>).path === "/docs",
    );
    expect(listings).toHaveLength(1);
    const applied = intentReport("i-nav");
    expect(applied).toMatchObject({ intentId: "i-nav", status: "applied" });
    const summary = applied!.summary!;
    expect(summary.panel).toBe("browse");
    expect(summary.count).toBeGreaterThan(0);
    // path 定位字段不截断：摘要行 path 与列表前 5 行全等。
    expect(summary.rows).toEqual(
      (table().props("entries") as Array<{ path: string }>).slice(0, 5).map((entry) => expect.objectContaining({ path: entry.path })),
    );
    expect(table().props("entries").some((entry: { path: string }) => entry.path === "/docs/readme.md")).toBe(true);
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("en", "intent.applied"));
  });

  it("switches panels on focus and rejects unknown panels", async () => {
    mountWorkbench();
    await settle();
    invoke.mockClear();
    emitUiIntent({ intentId: "i-audit", action: "focus", params: { panel: "audit" } });
    await settle();
    expect(wrapper!.find(".wb-dock").exists()).toBe(true);
    expect(intentReport("i-audit")).toMatchObject({ intentId: "i-audit", status: "applied", summary: { panel: "audit" } });
    emitUiIntent({ intentId: "i-browse", action: "focus", params: { panel: "browse" } });
    await settle();
    expect(wrapper!.find(".wb-dock").exists()).toBe(false);
    expect(intentReport("i-browse")).toMatchObject({ intentId: "i-browse", status: "applied", summary: { panel: "browse" } });
    emitUiIntent({ intentId: "i-bad", action: "focus", params: { panel: "hidden" } });
    await settle();
    expect(intentReport("i-bad")).toMatchObject({
      intentId: "i-bad",
      status: "rejected",
      summary: { reason: workbenchMessage("en", "intent.unknownPanel") },
    });
  });

  it("locates and highlights a row by path, rejecting missing paths with a localized reason", async () => {
    mountWorkbench();
    await settle();
    invoke.mockClear();
    // 先导航进 /docs（AI select 面向当前列表的行定位）。
    const docs = (table().props("entries") as Array<{ path: string; kind: string }>).find((entry) => entry.path === "/docs")!;
    table().vm.$emit("open", docs);
    await settle();
    emitUiIntent({ intentId: "i-select", action: "select", params: { path: "/docs/readme.md" } });
    await settle();
    expect(table().props("selection")).toEqual(["/docs/readme.md"]);
    expect(table().props("activePath")).toBe("/docs/readme.md");
    expect(intentReport("i-select")).toMatchObject({
      intentId: "i-select",
      status: "applied",
      summary: { count: 1, anchor: "/docs/readme.md", rows: [expect.objectContaining({ path: "/docs/readme.md" })] },
    });
    emitUiIntent({ intentId: "i-miss", action: "select", params: { path: "/nowhere/ghost.txt" } });
    await settle();
    expect(intentReport("i-miss")).toMatchObject({
      intentId: "i-miss",
      status: "rejected",
      summary: { reason: workbenchMessage("en", "intent.selectMissing") },
    });
  });

  it("pushes snapshot reports after navigation and selection changes", async () => {
    mountWorkbench();
    await settle();
    invoke.mockClear();
    const docs = (table().props("entries") as Array<{ path: string; kind: string }>).find((entry) => entry.path === "/docs")!;
    table().vm.$emit("open", docs);
    await settle();
    expect(reportBodies()).toContainEqual(
      expect.objectContaining({ status: "snapshot", summary: expect.objectContaining({ panel: "browse", path: "/docs", count: expect.any(Number) }) }),
    );
    invoke.mockClear();
    table().vm.$emit("update:selection", ["/docs/readme.md"]);
    await nextTick();
    expect(reportBodies()).toContainEqual(
      expect.objectContaining({ status: "snapshot", summary: expect.objectContaining({ path: "/docs", anchor: "/docs/readme.md" }) }),
    );
  });

  it("reports rejected for actions without a handler instead of stalling the intent", async () => {
    mountWorkbench();
    await settle();
    invoke.mockClear();
    emitUiIntent({ intentId: "i-teleport", action: "teleport", params: {} });
    await settle();
    expect(reportBodies()).toContainEqual({
      intentId: "i-teleport",
      status: "rejected",
      summary: { reason: 'no handler for action "teleport"' },
    });
  });
});

describe("mock report contract mirror", () => {
  it("mirrors the sidecar report contract: applied/rejected accepted, snapshot tolerated, bad status refused", async () => {
    await expect(window.dbxPlugin.invoke("files/ui/state/report", { intentId: "i-1", status: "applied", summary: { count: 1 } })).resolves.toEqual({ success: true });
    await expect(window.dbxPlugin.invoke("files/ui/state/report", { intentId: "i-1", status: "rejected", summary: { reason: "x" } })).resolves.toEqual({ success: true });
    await expect(window.dbxPlugin.invoke("files/ui/state/report", { status: "snapshot", summary: { panel: "browse" } })).resolves.toEqual({ success: true });
    await expect(window.dbxPlugin.invoke("files/ui/state/report", { intentId: "i-1", status: "pending" })).rejects.toThrow("status must be applied or rejected");
  });
});
