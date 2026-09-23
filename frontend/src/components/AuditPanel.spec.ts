// @vitest-environment happy-dom
// AuditPanel 操作类型筛选：候选来自已加载条目、筛选即时生效、选择持久化到
// ui prefs（跨刷新恢复由 prefs sanitize 白名单保证）。
import { afterEach, describe, expect, it, vi } from "vitest";
import { config, mount } from "@vue/test-utils";
import AuditPanel from "./AuditPanel.vue";
import { vTip } from "../lib/tooltip";
import { loadUiPrefs, prefsStore, saveUiPrefs, UI_PREFS_KEY } from "../lib/prefs";

config.global.directives = { tip: { mounted() {}, updated() {} } };

const t = (key: string) => key;

const ENTRIES = [
  { time: "2026-09-21T10:00:00Z", connectionId: "c1", action: "files/mkdir", target: "/a", result: "ok" },
  { time: "2026-09-21T10:01:00Z", connectionId: "c1", action: "files/write", target: "/b", result: "ok" },
  { time: "2026-09-21T10:02:00Z", connectionId: "c1", action: "files/mkdir", target: "/c", result: "ok" },
];

function panel() {
  window.dbxPlugin = {
    invoke: vi.fn().mockResolvedValue({ entries: ENTRIES }),
  } as unknown as Window["dbxPlugin"];
  return mount(AuditPanel, { props: { t } });
}

afterEach(() => {
  vi.restoreAllMocks();
  // 默认持久化后端是 prefsStore（宿主 storage 适配），清理须走同一实例。
  prefsStore.removeItem(UI_PREFS_KEY);
  Reflect.deleteProperty(window.dbxPlugin ?? {}, "invoke");
});

describe("AuditPanel operation filter", () => {
  it("derives options from entries and filters the list", async () => {
    prefsStore.setItem(UI_PREFS_KEY, JSON.stringify({}));
    const wrapper = panel();
    await vi.waitFor(() => expect(wrapper.findAll(".wb-audit-item").length).toBe(3));
    const options = wrapper.findAll(".wb-audit-filter option").map((o) => o.text());
    expect(options).toEqual(["auditFilterAll", "files/mkdir", "files/write"]);
    await wrapper.get(".wb-audit-filter select").setValue("files/mkdir");
    expect(wrapper.findAll(".wb-audit-item").length).toBe(2);
    // 持久化：选择写入 ui prefs。
    const stored = JSON.parse(prefsStore.getItem(UI_PREFS_KEY) ?? "{}");
    expect(stored.auditActionFilter).toBe("files/mkdir");
  });

  it("restores the persisted filter on mount", async () => {
    prefsStore.setItem(UI_PREFS_KEY, JSON.stringify({ auditActionFilter: "files/write" }));
    const wrapper = panel();
    await vi.waitFor(() => expect(wrapper.findAll(".wb-audit-item").length).toBe(1));
    expect((wrapper.get(".wb-audit-filter select").element as HTMLSelectElement).value).toBe("files/write");
  });

  it("shows the no-match state when the persisted filter matches nothing", async () => {
    prefsStore.setItem(UI_PREFS_KEY, JSON.stringify({ auditActionFilter: "files/nope" }));
    const wrapper = panel();
    await vi.waitFor(() => expect(wrapper.text()).toContain("auditFilterNoMatch"));
    expect(wrapper.findAll(".wb-audit-item").length).toBe(0);
  });
});

