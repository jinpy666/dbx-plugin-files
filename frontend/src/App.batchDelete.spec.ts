// @vitest-environment happy-dom
// 批量删除部分失败（P2-7 并发批次）：批次中任一目标失败时，已成功删除的
// 条目仍要通过目录刷新如实反映（失败不吞成功），错误横幅保留失败原因——
// 此前 runBatch 抛错直接进 catch 跳过 loadDirectory，列表停在删除前状态，
// 用户看到「删除后文件还在」。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import { installMockHost } from "./lib/mockHost";
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

function leftTable() {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === "left")!;
}

function leftRowTexts(): string[] {
  return leftTable().findAll('[role="option"]').map((row) => row.text());
}

function dialog() {
  return wrapper!.find("[role=dialog]");
}

describe("batch delete with a failing target", () => {
  it("refreshes the directory after a partial failure and keeps the error banner", async () => {
    mountWorkbench();
    await settle();
    // 借用 mock 树的 /docs 既有文件：data.csv 删除成功，notes.txt 注入失败。
    const rawInvoke = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    window.dbxPlugin.invoke = async (method: string, payload: Record<string, unknown>) => {
      if (method === "files/delete" && String(payload?.path).endsWith("/notes.txt")) {
        throw new Error("boom delete");
      }
      return rawInvoke(method, payload);
    };
    await leftTable().vm.$emit("open", {
      name: "docs",
      path: "/docs",
      kind: "directory",
      size: 0,
      modifiedAt: new Date().toISOString(),
    });
    await settle();
    expect(leftRowTexts().join("|")).toContain("notes.txt");

    await leftTable().vm.$emit("update:selection", ["/docs/data.csv", "/docs/notes.txt"]);
    await nextTick();
    leftTable().vm.$emit("delete");
    await nextTick();
    expect(dialog().text()).toContain("2");

    (dialog().findAll("button").find((b) => b.text() === "Confirm")!).trigger("click");
    await settle();

    // 失败不吞成功：已删除的 data.csv 必须从列表消失（目录已刷新）。
    const rows = leftRowTexts().join("|");
    expect(rows).not.toContain("data.csv");
    expect(rows).toContain("notes.txt");
    // 错误横幅保留失败原因。
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(true);
    expect(wrapper!.find(".wb-error-banner").text()).toContain("boom delete");
  });
});
