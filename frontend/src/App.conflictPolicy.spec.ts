// @vitest-environment happy-dom
// 传输同名冲突策略（对标 ssh 插件 downloadConflictPolicy 三档，上传+下载共用）
// 集成冒烟：上传预检（一次 list）→ ask 弹三键弹窗（覆盖/重命名/取消）/
// rename 自动让位 `name (n).ext` / overwrite 直传；下载落盘 ask 档经
// files/local/exists 探测后询问，覆盖透传 conflict 参数。真实 App + mock 桥。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import { installMockHost, seedMockLocalDownload } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { CONFLICT_POLICY_KEY, prefsStore } from "./lib/prefs";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";
import type { FileEntry } from "./lib/api";

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
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1");
  installMockHost();
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  // prefsStore 带内存写穿缓存，localStorage.clear() 清不掉——必须显式删键，
  // 否则前一个用例 setItem 的策略值会泄漏进后续 ask 用例（假绿/假红）。
  prefsStore.removeItem(CONFLICT_POLICY_KEY);
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

/** 记录 upload/start 与 download/start 调用，其余方法照常走 mock 宿主。 */
function stubStarts() {
  const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
  const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
  vi.spyOn(window.dbxPlugin, "invoke").mockImplementation((async (
    method: string,
    params?: Record<string, unknown>,
    options?: { timeoutMs?: number },
  ) => {
    if (method === "files/upload/start" || method === "files/download/start") {
      calls.push({ method, params: params ?? {} });
    }
    return raw(method, params, options);
  }) as typeof window.dbxPlugin.invoke);
  return calls;
}

function pasteEvent(file: File) {
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", { value: { files: [file], items: [], getData: () => "" } });
  return event;
}

async function pasteUpload(file: File) {
  document.dispatchEvent(pasteEvent(file));
  await settle();
}

function conflictDialog() {
  return wrapper!.find(".wb-dialog-backdrop");
}

function seedRemoteFile(name: string) {
  return window.dbxPlugin.invoke("files/write", {
    path: `/${name}`,
    dataBase64: window.dbxPlugin.encodeBase64(new Uint8Array([65])),
    connectionId: "mock-conn",
  });
}

describe("upload conflict policy", () => {
  it("asks on conflict in ask mode and overwrites when chosen", async () => {
    await seedRemoteFile("dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await pasteUpload(new File(["A"], "dup.txt"));
    expect(conflictDialog().exists()).toBe(true);
    expect(calls).toEqual([]);
    await conflictDialog().get(".wb-dialog-danger").trigger("click");
    await settle();
    expect(conflictDialog().exists()).toBe(false);
    expect(calls).toHaveLength(1);
    expect(calls[0].params.remotePath).toBe("/dup.txt");
  });

  it("renames to name (1).ext when rename is chosen in the dialog", async () => {
    await seedRemoteFile("dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await pasteUpload(new File(["A"], "dup.txt"));
    await conflictDialog().get(".wb-dialog-primary").trigger("click");
    await settle();
    expect(calls).toHaveLength(1);
    expect(calls[0].params.remotePath).toBe("/dup (1).txt");
  });

  it("aborts the whole batch when the dialog is cancelled", async () => {
    await seedRemoteFile("dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await pasteUpload(new File(["A"], "dup.txt"));
    await conflictDialog().get(".wb-dialog-cancel").trigger("click");
    await settle();
    expect(conflictDialog().exists()).toBe(false);
    expect(calls).toEqual([]);
  });

  it("renames silently in rename mode without asking", async () => {
    prefsStore.setItem(CONFLICT_POLICY_KEY, "rename");
    await seedRemoteFile("dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await pasteUpload(new File(["A"], "dup.txt"));
    expect(conflictDialog().exists()).toBe(false);
    expect(calls).toHaveLength(1);
    expect(calls[0].params.remotePath).toBe("/dup (1).txt");
  });

  it("passes through untouched in overwrite mode without asking", async () => {
    prefsStore.setItem(CONFLICT_POLICY_KEY, "overwrite");
    await seedRemoteFile("dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await pasteUpload(new File(["A"], "dup.txt"));
    expect(conflictDialog().exists()).toBe(false);
    expect(calls).toHaveLength(1);
    expect(calls[0].params.remotePath).toBe("/dup.txt");
  });

  it("does not disturb a conflict-free upload in ask mode", async () => {
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await pasteUpload(new File(["A"], "fresh.txt"));
    expect(conflictDialog().exists()).toBe(false);
    expect(calls).toHaveLength(1);
    expect(calls[0].params.remotePath).toBe("/fresh.txt");
  });
});

describe("download conflict policy (local save)", () => {
  async function selectAndDownload(name: string) {
    const table = wrapper!.findAllComponents(FileTable)[0]!;
    const entry = (table.props("entries") as FileEntry[]).find((candidate) => candidate.name === name);
    expect(entry, `remote fixture ${name} should be listed`).toBeTruthy();
    // selection 的 payload 是路径数组（见 App.parityTools.spec 先例），不是条目对象。
    table.vm.$emit("update:selection", [entry!.path]);
    await settle();
    const download = wrapper!.getComponent(FileToolbar).findAll("button")
      .find((button) => button.attributes("aria-label") === workbenchMessage("en", "download"))!;
    await download.trigger("click");
    // 冲突链路是 downloadSelection → probe → exists → askFileConflict 的
    // 多段异步，等两轮再断言弹窗。
    await settle();
    await settle();
  }

  it("probes the download dir, asks, and passes overwrite to the sidecar", async () => {
    await seedRemoteFile("dl-dup.txt");
    seedMockLocalDownload("/Users/demo/Downloads/dl-dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await selectAndDownload("dl-dup.txt");
    expect(conflictDialog().exists()).toBe(true);
    expect(calls).toEqual([]);
    await conflictDialog().get(".wb-dialog-danger").trigger("click");
    await settle();
    const start = calls.find((call) => call.method === "files/download/start");
    expect(start?.params).toMatchObject({ conflict: "overwrite", saveToLocal: true });
  });

  it("downloads without asking in rename mode (sidecar default naming)", async () => {
    prefsStore.setItem(CONFLICT_POLICY_KEY, "rename");
    await seedRemoteFile("dl-dup.txt");
    seedMockLocalDownload("/Users/demo/Downloads/dl-dup.txt");
    mountWorkbench();
    await settle();
    const calls = stubStarts();
    await selectAndDownload("dl-dup.txt");
    expect(conflictDialog().exists()).toBe(false);
    const start = calls.find((call) => call.method === "files/download/start");
    expect(start?.params.conflict).toBeUndefined();
  });
});
