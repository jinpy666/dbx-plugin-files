// @vitest-environment happy-dom
// UI smoke：真实 App + FileTable + 确认弹层 + mock 桥，不触达真实文件或连接。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { defineComponent, nextTick, ref, type Component } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import TransferPanel from "./components/TransferPanel.vue";
import SideNavPanel from "./components/SideNavPanel.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import { currentConnectionId, parentPath, joinPath, type FileEntry } from "./lib/api";

// 审计#7：可编程的 PreviewPane 桩——真实组件的 isDirty 计算由 PreviewPane.spec
// 覆盖，这里用 stub 驱动 App 层的关闭守卫接线（Esc → 丢弃确认 → 丢弃/保留）。
const PreviewPaneStub = defineComponent({
  props: {
    path: { type: String, default: null },
    canWrite: Boolean,
    connectionId: { type: String, default: undefined },
    t: { type: Function, required: true },
  },
  emits: ["close", "saved", "download"],
  setup(_props, { expose }) {
    const isDirty = ref(false);
    expose({ isDirty });
    return { isDirty };
  },
  template: `<div class="stub-preview"><button data-test="set-dirty" @click="isDirty = true">dirty</button><button data-test="stub-close" @click="$emit('close')">close</button></div>`,
});

let wrapper: VueWrapper | undefined;
let host: NonNullable<ReturnType<typeof installMockHost>>;

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
  host = installMockHost()!;
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "dbxPlugin");
});

function mountWorkbench(stubs?: Record<string, boolean | Component>) {
  wrapper = mount(App, { attachTo: document.body, global: { directives: { tip: vTip }, stubs } });
  return wrapper;
}

async function settle() {
  for (let i = 0; i < 8; i++) {
    await vi.advanceTimersByTimeAsync(1);
    await nextTick();
  }
}

function table(side: "left" | "right") {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === side)!;
}

async function openDualPane() {
  const button = wrapper!.getComponent(FileToolbar).findAll("button")
    .find((button) => button.attributes("aria-label") === workbenchMessage("en", "dualPane"))!;
  await button.trigger("click");
  await settle();
}

async function selectEntry(side: "left" | "right", entry: FileEntry) {
  const row = table(side).findAll(".wb-file-row").find((row) =>
    row.find(".wb-file-name span").attributes("title") === entry.path,
  )!;
  await row.trigger("click");
}

describe("fresh review UI smoke", () => {
  it("shows loading until the first listing resolves, then distinguishes a genuinely empty folder", async () => {
    mountWorkbench();
    expect(table("left").props("loading")).toBe(true);
    expect(table("left").text()).not.toContain(workbenchMessage("en", "emptyDirectory"));
    await settle();
    const empty = table("left").props("entries").find((entry) => entry.path === "/empty")!;
    table("left").vm.$emit("open", empty);
    await settle();
    expect(table("left").get(".wb-file-empty").text()).toBe(workbenchMessage("en", "emptyDirectory"));
  });

  it("keeps left and right side navigation tabs independent", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const panels = () => wrapper!.findAllComponents(SideNavPanel);
    expect(panels()[0].props("tab")).toBe("quick");
    expect(panels()[1].props("tab")).toBe("tree");
    await panels()[0].findAll(".wb-side-tabs button")[0].trigger("click");
    await nextTick();
    expect(panels()[0].props("tab")).toBe("tree");
    expect(panels()[1].props("tab")).toBe("tree");
    const rightTree = panels()[1].props("treeRoot");
    expect(rightTree).not.toBeNull();
    if (!rightTree) throw new Error("right tree root is missing");
    expect(rightTree.loaded).toBe(true);
    expect(rightTree.expanded).toBe(true);
    expect(rightTree.children.map((node: { path: string }) => node.path)).toContain("/docs");
    await panels()[1].findAll(".wb-side-tabs button")[1].trigger("click");
    await nextTick();
    expect(panels()[0].props("tab")).toBe("tree");
    expect(panels()[1].props("tab")).toBe("quick");
  });

  it("loads the right pane on first open even while the local pane is waiting", async () => {
    mountWorkbench();
    await settle();
    const original = window.dbxPlugin.invoke;
    let releaseLocal!: () => void;
    const gate = new Promise<void>((resolve) => { releaseLocal = resolve; });
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/list" && (params as Record<string, unknown>).connectionId === "__local__") await gate;
      return original<T>(method, params);
    });
    await openDualPane();
    expect(table("left").props("loading")).toBe(true);
    expect(table("right").props("loading")).toBe(false);
    expect(table("right").props("entries").length).toBeGreaterThan(0);
    releaseLocal();
    await settle();
    expect(table("left").props("loading")).toBe(false);
  });

  it.each(["left", "right"] as const)("keeps a failed %s listing distinct from empty and retries that pane", async (side) => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const entry = table(side).props("entries")[0];
    await selectEntry(side, entry);
    const original = window.dbxPlugin.invoke;
    let fail = true;
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      const local = (params as Record<string, unknown> | undefined)?.connectionId === "__local__";
      if (fail && method === "files/list" && local === (side === "left")) throw new Error("connection refused");
      return original<T>(method, params);
    });
    table(side).vm.$emit("retry");
    await settle();
    expect(table(side).props("failed")).toBe(true);
    expect(table(side).props("selection")).toEqual([]);
    expect(table(side).findAll(".wb-file-row")).toHaveLength(0);
    expect(table(side).text()).toContain(workbenchMessage("en", "directoryLoadFailed"));
    expect(table(side === "left" ? "right" : "left").props("failed")).toBe(false);
    expect(wrapper!.text()).toContain("Check the server address and network, then reconnect.");
    fail = false;
    invoke.mockClear();
    await table(side).get("[role=status] button").trigger("click");
    await settle();
    expect(table(side).props("failed")).toBe(false);
    expect(table(side).props("entries").length).toBeGreaterThan(0);
    const listings = invoke.mock.calls.filter(([method]) => method === "files/list");
    expect(listings).toHaveLength(1);
    expect(listings[0][1]).toMatchObject({ connectionId: side === "left" ? "__local__" : "mock-conn" });
  });

  it.each(["left", "right"] as const)("routes F2 and confirmed Delete to the %s connection", async (side) => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const folder = parentPath(table(side).props("entries")[0].path);
    await window.dbxPlugin.invoke("files/write", {
      path: joinPath(folder, "shortcut-round4.txt"), dataBase64: "aGk=",
      connectionId: side === "left" ? "__local__" : "mock-conn",
    });
    table(side).vm.$emit("retry");
    await settle();
    const invoke = vi.spyOn(window.dbxPlugin, "invoke");
    const entry = table(side).props("entries").find((entry) => entry.name === "shortcut-round4.txt")!;
    await selectEntry(side, entry);
    await table(side).get(".wb-file-scroll").trigger("keydown", { key: "F2" });
    expect(wrapper!.get("[role=dialog] input").element).toHaveProperty("value", entry.name);
    expect(invoke.mock.calls.some(([method]) => method === "files/rename")).toBe(false);
    const newPath = joinPath(parentPath(entry.path), "renamed-round4.txt");
    await wrapper!.get("[role=dialog] input").setValue("renamed-round4.txt");
    await wrapper!.get("[role=dialog] footer button:last-child").trigger("click");
    await settle();
    expect(invoke.mock.calls.find(([method]) => method === "files/rename")?.[1]).toMatchObject({
      path: entry.path, newPath, connectionId: side === "left" ? "__local__" : "mock-conn",
    });
    const renamed = table(side).props("entries").find((entry) => entry.path === newPath)!;
    await selectEntry(side, renamed);
    await table(side).get(".wb-file-scroll").trigger("keydown", { key: "Delete" });
    expect(wrapper!.find("[role=dialog]").exists()).toBe(true);
    expect(invoke.mock.calls.some(([method]) => method === "files/delete")).toBe(false);
    await wrapper!.get("[role=dialog] footer button:last-child").trigger("click");
    await settle();
    expect(invoke.mock.calls.find(([method]) => method === "files/delete")?.[1]).toMatchObject({
      path: newPath, connectionId: side === "left" ? "__local__" : "mock-conn",
    });
  });
});

async function showTransfers() {
  if (!wrapper!.findComponent(TransferPanel).exists()) {
    wrapper!.getComponent(FileToolbar).vm.$emit("toggle-dock", "transfers");
    await nextTick();
  }
  return wrapper!.getComponent(TransferPanel);
}

describe("toolbar interaction safeguards", () => {
  it("blocks the browser context menu on the global and dock toolbars", async () => {
    mountWorkbench();
    await settle();
    const toolbarEvent = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    wrapper!.get(".wb-toolbar").element.dispatchEvent(toolbarEvent);
    expect(toolbarEvent.defaultPrevented).toBe(true);

    await showTransfers();
    const dockEvent = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    wrapper!.get(".wb-dock-tabs").element.dispatchEvent(dockEvent);
    expect(dockEvent.defaultPrevented).toBe(true);
  });

  it("shows an error banner when move returns an explicit failure response", async () => {
    mountWorkbench();
    await settle();
    const entry = table("left").props("entries").find((item) => item.path === "/backup.zip")!;
    table("left").vm.$emit("contextmenu", { entry, x: 10, y: 10 });
    await nextTick();
    await wrapper!.findAll("[role=menuitem]").find((item) => item.text() === `${workbenchMessage("en", "transferKind.move")}…`)!.trigger("click");
    await wrapper!.get("[role=dialog] input").setValue("/moved-readme.md");

    const original = window.dbxPlugin.invoke;
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/move") return { success: false, error: "permission denied" } as T;
      return original<T>(method, params);
    });
    await wrapper!.get("[role=dialog] footer button:last-child").trigger("click");
    await settle();

    expect(wrapper!.get(".wb-error-banner").text()).toContain(workbenchMessage("en", "errPermission"));
  });
});

describe("failed transfer feedback", () => {
  it("keeps the error banner visible when an async move fails", async () => {
    mountWorkbench();
    await settle();
    await submitFailedTransfer("move");
    expect(wrapper!.get(".wb-error-banner").text()).toContain("mock job failure");
  });
});

type RetryKind = "copy" | "move" | "rename" | "copyDir" | "syncDir";

async function submitFailedTransfer(kind: RetryKind, source = "/docs") {
  const entry = table("left").props("entries").find((item) => item.path === source)!;
  table("left").vm.$emit("contextmenu", { entry, x: 10, y: 10 });
  await nextTick();
  const label = kind === "rename" ? workbenchMessage("en", "rename") : `${workbenchMessage("en", `transferKind.${kind}`)}…`;
  await wrapper!.findAll("[role=menuitem]").find((item) => item.text() === label)!.trigger("click");
  // SyncDialog（copyDir/syncDir）第一个输入框是可编辑源路径，目标路径按
  // aria-label 定位；其余确认弹窗只有单个输入框。
  const dialogInput = kind === "copyDir" || kind === "syncDir"
    ? wrapper!.get('[role=dialog] input[aria-label="Target path"]')
    : wrapper!.get("[role=dialog] input");
  await dialogInput.setValue(kind === "rename" ? `failed-${kind}` : `/failed-${kind}`);
  await wrapper!.get("[role=dialog] footer button:last-child").trigger("click");
  await settle();
  await vi.advanceTimersByTimeAsync(450);
  await settle();
  const panel = await showTransfers();
  const job = panel.props("jobs").find((item) => item.kind === kind && item.state === "failed")!;
  expect(job).toBeDefined();
  expect(panel.props("retryableIds")).toContain(job.jobId);
  return job;
}

async function connectFixture(id: string, readOnly = false) {
  await window.dbxPlugin.invoke("connection/connect", {
    connection: { id, external_config: { protocol: "fs", root: "/mock" } },
  });
  await window.dbxPlugin.invoke("files/write", { connectionId: id, path: `/${id}.txt`, dataBase64: "" });
  if (readOnly) await window.dbxPlugin.invoke("connection/connect", { connection: { id, read_only: true, external_config: { protocol: "fs", root: "/mock" } } });
}

describe("round5 retry UI smoke", () => {
  it.each(["copy", "move", "rename", "copyDir", "syncDir"] as const)("replays %s against its original connection after pane and host changes", async (kind) => {
    mountWorkbench();
    await settle();
    const invoke = vi.spyOn(window.dbxPlugin, "invoke");
    const job = await submitFailedTransfer(kind);
    const original = invoke.mock.calls.find(([method]) => method === `files/${kind}`)![1];
    expect(original).toMatchObject({ connectionId: "mock-conn" });
    if (kind === "copyDir" || kind === "syncDir") {
      expect(original).toMatchObject({ sourceConnectionId: "mock-conn", targetConnectionId: "mock-conn" });
    }
    await openDualPane();
    await connectFixture("other");
    host.setContext({ connectionId: "other" });
    await settle();
    expect(currentConnectionId()).toBe("other");
    const panel = await showTransfers();
    await panel.get(`[aria-label="${workbenchMessage("en", "retryTransfer")}"]`).trigger("click");
    await settle();
    const submissions = invoke.mock.calls.filter(([method]) => method === `files/${kind}`);
    expect(submissions).toHaveLength(2);
    expect(submissions[1][1]).toEqual(original);
    expect(panel.props("retryableIds")).not.toContain(job.jobId);
    expect(panel.props("jobs").some((item) => item.jobId !== job.jobId && item.kind === kind && item.state === "queued")).toBe(true);
  });

  it.each([
    { from: "left", kind: "copy" }, { from: "right", kind: "copy" },
    { from: "left", kind: "move" }, { from: "right", kind: "move" },
  ] as const)("registers $from cross-pane $kind retries with both connection IDs", async ({ from, kind }) => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const folder = parentPath(table(from).props("entries")[0].path);
    const paths = ["failed-one.txt", "failed-two.txt"].map((name) => joinPath(folder, name));
    for (const path of paths) await window.dbxPlugin.invoke("files/write", { connectionId: from === "left" ? "__local__" : "mock-conn", path, dataBase64: "" });
    table(from).vm.$emit("retry");
    await settle();
    table(from).vm.$emit("update:selection", paths);
    await nextTick();
    const entry = table(from).props("entries").find((item) => item.path === paths[0])!;
    table(from).vm.$emit("contextmenu", { entry, x: 10, y: 10 });
    await nextTick();
    const invoke = vi.spyOn(window.dbxPlugin, "invoke");
    const label = workbenchMessage("en", kind === "copy" ? "copyToTarget" : "moveToTarget");
    await wrapper!.findAll("[role=menuitem]").find((item) => item.text() === label)!.trigger("click");
    await settle();
    await vi.advanceTimersByTimeAsync(450);
    await settle();
    const panel = await showTransfers();
    expect(panel.props("retryableIds")).toHaveLength(2);
    const job = panel.props("jobs").find((item) => item.remotePath?.startsWith(`${paths[0]} →`))!;
    const original = invoke.mock.calls.find(([method, params]) => method === `files/${kind}` && (params as Record<string, unknown>).sourcePath === paths[0])![1];
    expect(original).toMatchObject({ sourceConnectionId: from === "left" ? "__local__" : "mock-conn", targetConnectionId: from === "left" ? "mock-conn" : "__local__" });
    await wrapper!.get(".wb-pane-source select").setValue("");
    await connectFixture("other");
    host.setContext({ connectionId: "other" });
    await settle();
    panel.vm.$emit("retry", job.jobId);
    await settle();
    expect(invoke.mock.calls.filter(([method]) => method === `files/${kind}`).at(-1)![1]).toEqual(original);
  });

  it("accepts synchronous retry completion, refreshes the listing and keeps the notice translatable", async () => {
    Reflect.deleteProperty(window, "dbxPlugin");
    window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&job=1");
    host = installMockHost()!;
    mountWorkbench();
    await settle();
    const job = await submitFailedTransfer("copy", "/backup.zip");
    const original = window.dbxPlugin.invoke;
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/copy") {
        await original("files/write", { connectionId: "mock-conn", path: "/failed-copy", dataBase64: "" });
        return { success: true, transport: "native", jobId: null } as T;
      }
      return original<T>(method, params);
    });
    const panel = await showTransfers();
    await panel.get(`[aria-label="${workbenchMessage("en", "retryTransfer")}"]`).trigger("click");
    await settle();
    expect(invoke.mock.calls.some(([method]) => method === "files/list")).toBe(true);
    expect(table("left").props("entries").some((entry) => entry.path === "/failed-copy")).toBe(true);
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("en", "transferStatus.completed"));
    expect(panel.props("retryableIds")).not.toContain(job.jobId);
    expect(panel.props("jobs")).toHaveLength(1);
    host.setEnvironment({ locale: "ja" });
    await nextTick();
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("ja", "transferStatus.completed"));
  });

  it("retains retry after an invalid response and suppresses duplicate in-flight submissions", async () => {
    mountWorkbench();
    await settle();
    const job = await submitFailedTransfer("copy");
    const original = window.dbxPlugin.invoke;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/copy") {
        await gate;
        return { success: true, transport: "job", jobId: null } as T;
      }
      return original<T>(method, params);
    });
    const panel = await showTransfers();
    panel.vm.$emit("retry", job.jobId);
    panel.vm.$emit("retry", job.jobId);
    await settle();
    expect(invoke.mock.calls.filter(([method]) => method === "files/copy")).toHaveLength(1);
    release();
    await settle();
    expect(panel.props("retryableIds")).toContain(job.jobId);
    expect(wrapper!.get(".wb-error-banner").text()).toContain(workbenchMessage("en", "featureMissing", { method: "files/copy" }));
  });
});

describe("round5 current host context/environment UI smoke", () => {
  it("handles init after the host.getContext fallback has already loaded the workbench", async () => {
    await connectFixture("other");
    let initialize!: (context: Record<string, unknown>) => void;
    const stopInit = vi.fn();
    vi.spyOn(window.dbxPlugin, "onInit").mockImplementation((listener) => {
      initialize = listener;
      return stopInit;
    });
    mountWorkbench();
    await settle();
    vi.spyOn(window.dbxPlugin, "locale", "get").mockReturnValue("ja");
    initialize({ connectionId: "other" });
    await settle();
    expect(document.documentElement.lang).toBe("ja");
    expect(currentConnectionId()).toBe("other");
    expect(table("left").props("entries").map((entry) => entry.path)).toEqual(["/other.txt"]);
    wrapper!.unmount();
    wrapper = undefined;
    expect(stopInit).toHaveBeenCalledTimes(1);
  });

  it("still initializes when optional context/init subscriptions are absent", async () => {
    Reflect.deleteProperty(window.dbxPlugin, "onContext");
    Reflect.deleteProperty(window.dbxPlugin, "onInit");
    mountWorkbench();
    await settle();
    expect(table("left").props("loading")).toBe(false);
    expect(table("left").props("entries").length).toBeGreaterThan(0);
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
  });

  it("keeps confirmed batch deletion on the original connection during a host switch", async () => {
    await connectFixture("other");
    const paths = Array.from({ length: 9 }, (_, i) => `/delete-${i}.txt`);
    for (const connectionId of ["mock-conn", "other"]) {
      for (const path of paths) await window.dbxPlugin.invoke("files/write", { connectionId, path, dataBase64: "" });
    }
    mountWorkbench();
    await settle();
    table("left").vm.$emit("update:selection", paths);
    await nextTick();
    wrapper!.getComponent(FileToolbar).vm.$emit("delete");
    await nextTick();
    const original = window.dbxPlugin.invoke;
    let switched = false;
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/delete" && !switched) {
        switched = true;
        host.setContext({ connectionId: "other" });
      }
      return original<T>(method, params);
    });
    await wrapper!.get("[role=dialog] footer button:last-child").trigger("click");
    await settle();
    const deletes = invoke.mock.calls.filter(([method]) => method === "files/delete");
    expect(deletes).toHaveLength(9);
    expect(deletes.every(([, params]) => (params as Record<string, unknown>).connectionId === "mock-conn")).toBe(true);
    for (const path of paths) await expect(original("files/stat", { connectionId: "other", path })).resolves.toMatchObject({ entry: { path } });
  });

  it("keeps an upload permission failure visible after the destination refresh", async () => {
    mountWorkbench();
    await settle();
    const original = window.dbxPlugin.invoke;
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/upload/finish") throw new Error("PermissionDenied: permission denied");
      return original<T>(method, params);
    });
    wrapper!.getComponent(FileToolbar).vm.$emit("upload", [new File(["A"], "failed-upload.txt")]);
    await settle();
    expect(invoke.mock.calls.some(([method]) => method === "files/upload/start")).toBe(true);
    expect(invoke.mock.calls.some(([method]) => method === "files/upload/finish")).toBe(true);
    expect(wrapper!.get(".wb-error-banner").text()).toContain(workbenchMessage("en", "errPermission"));
    expect(wrapper!.find(".wb-notice").exists()).toBe(false);
    await vi.advanceTimersByTimeAsync(8001);
    await nextTick();
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
  });

  it("shows a banner when a sidecar transfer progress event reports failure", async () => {
    mountWorkbench();
    await settle();
    host.emitEvent("files/transfer/progress", {
      taskId: "sidecar-failed",
      kind: "upload",
      connectionId: "mock-conn",
      remotePath: "/home/www/.~超市电费.xlsx",
      state: "failed",
      error: "PermissionDenied (permanent): permission denied",
      size: 10,
      transferred: 0,
      total: 10,
    });
    await nextTick();
    expect(wrapper!.get(".wb-error-banner").text()).toContain(workbenchMessage("en", "errPermission"));
  });

  it("keeps a multi-file upload on the destination chosen before a host switch", async () => {
    await connectFixture("other");
    mountWorkbench();
    await settle();
    const original = window.dbxPlugin.invoke;
    let switched = false;
    const invoke = vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      const result = await original<T>(method, params);
      if (method === "files/upload/finish" && !switched) {
        switched = true;
        host.setContext({ connectionId: "other" });
      }
      return result;
    });
    wrapper!.getComponent(FileToolbar).vm.$emit("upload", [new File(["A"], "upload-one.txt"), new File(["B"], "upload-two.txt")]);
    await settle();
    const starts = invoke.mock.calls.filter(([method]) => method === "files/upload/start");
    expect(starts).toHaveLength(2);
    expect(starts.every(([, params]) => (params as Record<string, unknown>).connectionId === "mock-conn")).toBe(true);
    for (const path of ["/upload-one.txt", "/upload-two.txt"]) {
      await expect(original("files/stat", { connectionId: "mock-conn", path })).resolves.toMatchObject({ entry: { path, size: 1 } });
      await expect(original("files/stat", { connectionId: "other", path })).rejects.toThrow("NotFound");
    }
  });

  it("consumes environment-only locale changes through onEvent and unsubscribes on unmount", async () => {
    mountWorkbench();
    await settle();
    host.setEnvironment({ locale: "ja" });
    await nextTick();
    expect(document.documentElement.lang).toBe("ja");
    expect(wrapper!.get(".wb-search-input").attributes("placeholder")).toBe(workbenchMessage("ja", "searchPlaceholder"));
    wrapper!.unmount();
    wrapper = undefined;
    host.setEnvironment({ locale: "es" });
    host.setContext({ connectionId: "detached" });
    await settle();
    expect(document.documentElement.lang).toBe("ja");
    expect(currentConnectionId()).toBe("mock-conn");
  });

  it("rebinds the default connection, clears its caches and ignores late old listing failures", async () => {
    mountWorkbench();
    await settle();
    await connectFixture("other");
    const docs = table("left").props("entries").find((entry) => entry.path === "/docs")!;
    await selectEntry("left", docs);
    const original = window.dbxPlugin.invoke;
    let rejectOld!: (reason: Error) => void;
    const oldListing = new Promise<never>((_resolve, reject) => { rejectOld = reject; });
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      const p = params as Record<string, unknown>;
      if (method === "files/list" && p.connectionId === "mock-conn" && p.path === "/docs") return oldListing;
      return original<T>(method, params);
    });
    table("left").vm.$emit("open", docs);
    await settle();
    host.setContext({ connectionId: "other", connection: { name: "Other storage" } });
    await settle();
    expect(currentConnectionId()).toBe("other");
    expect(table("left").props("selection")).toEqual([]);
    expect(table("left").props("entries").map((entry) => entry.path)).toEqual(["/other.txt"]);
    expect(wrapper!.getComponent(SideNavPanel).props("treeRoot")?.loaded).toBe(false);
    expect(wrapper!.getComponent(SideNavPanel).props("quickPaths").map((item) => item.path)).toEqual(["/"]);
    rejectOld(new Error("connection refused"));
    await settle();
    expect(table("left").props("failed")).toBe(false);
    expect(wrapper!.getComponent(FileToolbar).props("connState")).toBe("connected");
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
  });

  it("keeps the independent local pane while reloading the host pane", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const localEntries = table("left").props("entries");
    await selectEntry("left", localEntries[0]);
    const localSelection = table("left").props("selection");
    await connectFixture("other");
    host.setContext({ connectionId: "other" });
    await settle();
    expect(table("left").props("entries")).toEqual(localEntries);
    expect(table("left").props("selection")).toEqual(localSelection);
    expect(table("right").props("entries").map((entry) => entry.path)).toEqual(["/other.txt"]);
  });

  it("does not apply old capabilities when context changes during initialization", async () => {
    await connectFixture("other", true);
    const original = window.dbxPlugin.invoke;
    let releaseOld!: () => void;
    const gate = new Promise<void>((resolve) => { releaseOld = resolve; });
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params: unknown) => {
      if (method === "files/capabilities" && (params as Record<string, unknown>).connectionId === "mock-conn") await gate;
      return original<T>(method, params);
    });
    mountWorkbench();
    await settle();
    host.setContext({ connectionId: "other" });
    await settle();
    expect(table("left").props("entries").map((entry) => entry.path)).toEqual(["/other.txt"]);
    expect(wrapper!.getComponent(FileToolbar).props("canWrite")).toBe(false);
    releaseOld();
    await settle();
    expect(wrapper!.getComponent(FileToolbar).props("canWrite")).toBe(false);
    expect(table("left").props("entries").map((entry) => entry.path)).toEqual(["/other.txt"]);
  });
});

describe("audit medium round: os drop, menu keys, dock tabs, preview dialog", () => {
  it("uploads OS files dropped on the target pane and redirects source-pane drops with a notice", async () => {
    mountWorkbench();
    await settle();
    await openDualPane();
    const invoke = vi.spyOn(window.dbxPlugin, "invoke");
    const dropWith = (files: File[]) => {
      const event = new Event("drop", { bubbles: true, cancelable: true });
      Object.defineProperty(event, "dataTransfer", { value: { getData: () => "", items: [], files } });
      return event;
    };
    wrapper!.get(".wb-pane-target").element.dispatchEvent(dropWith([new File(["A"], "os-drop.txt")]));
    await settle();
    expect(invoke.mock.calls.find(([method]) => method === "files/upload/start")?.[1]).toMatchObject({
      remotePath: "/os-drop.txt", size: 1,
    });
    invoke.mockClear();
    wrapper!.get(".wb-pane-source").element.dispatchEvent(dropWith([new File(["A"], "os-drop2.txt")]));
    await nextTick();
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("en", "dropToTargetPane"));
  });

  it("makes context menus keyboard reachable: first item focus, arrow navigation, Escape close", async () => {
    mountWorkbench();
    await settle();
    table("left").vm.$emit("contextmenu", { entry: table("left").props("entries")[0], x: 10, y: 10 });
    // 菜单 watch 内部还有一次 nextTick（钳位 + 聚焦首项），多轮 flush 等它跑完。
    await settle();
    const items = () => wrapper!.get(".wb-context-menu").findAll('[role="menuitem"]');
    expect(items().length).toBeGreaterThan(2);
    expect(document.activeElement).toBe(items()[0]!.element);
    await items()[0]!.trigger("keydown", { key: "ArrowDown" });
    expect(document.activeElement).toBe(items()[1]!.element);
    await items()[1]!.trigger("keydown", { key: "ArrowUp" });
    expect(document.activeElement).toBe(items()[0]!.element);
    await items()[0]!.trigger("keydown", { key: "End" });
    expect(document.activeElement).toBe(items().at(-1)!.element);
    await items().at(-1)!.trigger("keydown", { key: "Escape" });
    await nextTick();
    expect(wrapper!.find(".wb-context-menu").exists()).toBe(false);
  });

  it("gives dock tabs tablist semantics with roving focus and arrow-key switching", async () => {
    mountWorkbench();
    await settle();
    await showTransfers();
    const tabs = () => wrapper!.get(".wb-dock-tabs").findAll('[role="tab"]');
    // settings 已拆为独立设置弹窗；dock 为 transfers/stats/audit/connection 四页签。
    expect(tabs()).toHaveLength(4);
    expect(tabs()[0]!.attributes("aria-selected")).toBe("true");
    expect(tabs()[1]!.attributes("aria-selected")).toBe("false");
    expect(tabs()[1]!.attributes("tabindex")).toBe("-1");
    await tabs()[0]!.trigger("keydown", { key: "ArrowRight" });
    await nextTick();
    expect(tabs()[1]!.attributes("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(tabs()[1]!.element);
    await tabs()[1]!.trigger("keydown", { key: "ArrowLeft" });
    await nextTick();
    expect(tabs()[0]!.attributes("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(tabs()[0]!.element);
  });

  it("renders the preview as a modal dialog and confirms before discarding unsaved edits", async () => {
    mountWorkbench({ PreviewPane: PreviewPaneStub });
    await settle();
    const entry = table("left").props("entries").find((item) => item.kind === "file")!;
    table("left").vm.$emit("open", entry);
    await settle();
    const overlay = wrapper!.get(".wb-preview-overlay");
    expect(overlay.attributes("role")).toBe("dialog");
    expect(overlay.attributes("aria-modal")).toBe("true");
    expect(overlay.attributes("aria-label")).toBe(entry.name);
    // 焦点陷阱：末位按钮 Tab 回绕到首位（同 ConfirmDialog 实现）。
    const overlayButtons = () => wrapper!.get(".wb-preview-overlay").findAll("button");
    overlayButtons().at(-1)!.element.focus();
    await overlayButtons().at(-1)!.trigger("keydown", { key: "Tab" });
    expect(document.activeElement).toBe(overlayButtons()[0]!.element);
    // 干净态 Esc 直接关闭。
    await overlay.trigger("keydown", { key: "Escape" });
    await nextTick();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(false);
    // 脏草稿（isDirty）Esc → 先弹丢弃确认，不静默丢稿。
    table("left").vm.$emit("open", entry);
    await settle();
    await wrapper!.get("[data-test=set-dirty]").trigger("click");
    await wrapper!.get(".wb-preview-overlay").trigger("keydown", { key: "Escape" });
    await nextTick();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(true);
    const discardDialog = () => wrapper!.findAll(".wb-dialog").at(-1)!;
    expect(discardDialog().text()).toContain(workbenchMessage("en", "previewDiscardTitle"));
    // 「继续编辑」：确认层关闭，预览与草稿保留。
    await discardDialog().get("footer button:first-child").trigger("click");
    await nextTick();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(true);
    // 再 Esc → 「丢弃」：预览关闭。
    await wrapper!.get(".wb-preview-overlay").trigger("keydown", { key: "Escape" });
    await nextTick();
    await discardDialog().get("footer button:last-child").trigger("click");
    await nextTick();
    expect(wrapper!.find(".wb-preview-overlay").exists()).toBe(false);
  });
});
