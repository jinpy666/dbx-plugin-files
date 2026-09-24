// @vitest-environment happy-dom
// 文件夹上传（webkitdirectory 原生选择器，web 与桌面宿主同路径）：按
// webkitRelativePath 还原目录树——先建远端目录（已存在=合并），再逐文件走
// 统一上传泵；冲突策略按子目录分组生效（ask 带目录前缀、rename 目录内让位）。
// 真实 App + mock 桥。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileToolbar from "./components/FileToolbar.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { CONFLICT_POLICY_KEY, prefsStore, saveUiPrefs } from "./lib/prefs";
import { vTip } from "./lib/tooltip";

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
  // prefsStore 带内存写穿缓存，localStorage.clear() 清不掉——必须显式删键，
  // 否则前一个用例 setItem 的策略值会泄漏进后续用例（假绿/假红）。
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

function folderFile(relativePath: string, content: string): File {
  const file = new File([content], relativePath.split("/").pop()!, { type: "text/plain" });
  Object.defineProperty(file, "webkitRelativePath", { value: relativePath });
  return file;
}

/** 经 FileToolbar 的 webkitdirectory input 触发文件夹选择（File 携带相对路径）。 */
async function pickFolder(files: File[]) {
  const toolbar = wrapper!.getComponent(FileToolbar);
  const input = toolbar.find("input[webkitdirectory]");
  expect(input.exists(), "toolbar should render a webkitdirectory input").toBe(true);
  Object.defineProperty(input.element, "files", { value: files });
  await input.trigger("change");
}

async function listNames(path: string): Promise<string[]> {
  // fake timers：mock files/list 内部有 0ms delay timer，直接 await 会死等——
  // 先起调用再推进时钟。
  const pending = window.dbxPlugin.invoke<{ entries: Array<{ name: string }> }>("files/list", { path, connectionId: "mock-conn" });
  await vi.advanceTimersByTimeAsync(1);
  await nextTick();
  return (await pending).entries.map((entry) => entry.name);
}

describe("folder upload (webkitdirectory)", () => {
  it("restores the directory tree under the target root", async () => {
    mountWorkbench();
    await settle();
    await pickFolder([
      folderFile("docs/a.txt", "A"),
      folderFile("docs/sub/b.txt", "B"),
      folderFile("docs/c.txt", "C"),
    ]);
    await settle();
    await settle();
    expect(await listNames("/docs")).toEqual(expect.arrayContaining(["a.txt", "c.txt", "sub"]));
    expect(await listNames("/docs/sub")).toEqual(["b.txt"]);
    // 上传泵按 8 字节 offset 前缀 + 分块发送，落盘内容必须与源一致。
    const kept = await window.dbxPlugin.invoke<{ dataBase64: string; size: number }>("files/read", { path: "/docs/sub/b.txt", connectionId: "mock-conn" });
    expect(kept.size).toBe(1);
    expect(window.dbxPlugin.decodeBase64(kept.dataBase64)[0]).toBe(66); // "B"
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
    expect(wrapper!.find(".wb-notice").text())
      .toBe(workbenchMessage("en", "uploaded", { count: 3, path: "/" }));
  });

  it("asks on subdirectory conflicts with the dir prefix and cancelling keeps the original", async () => {
    await window.dbxPlugin.invoke("files/write", {
      path: "/docs/dup.txt",
      dataBase64: window.dbxPlugin.encodeBase64(new Uint8Array([90])), // "Z"
      connectionId: "mock-conn",
    });
    mountWorkbench();
    await settle();
    await pickFolder([folderFile("docs/dup.txt", "A")]);
    await settle();
    await settle();
    const dialog = wrapper!.find(".wb-dialog-backdrop");
    expect(dialog.exists()).toBe(true);
    // 撞名提示带相对目录前缀（区分是哪个子目录下的文件）。
    expect(dialog.text()).toContain("docs/dup.txt");
    await dialog.get(".wb-dialog-cancel").trigger("click");
    await settle();
    const kept = await window.dbxPlugin.invoke<{ dataBase64: string; size: number }>("files/read", { path: "/docs/dup.txt", connectionId: "mock-conn" });
    expect(kept.size).toBe(1);
    expect(window.dbxPlugin.decodeBase64(kept.dataBase64)[0]).toBe(90);
  });

  it("renames colliding files in place under the rename policy without asking", async () => {
    prefsStore.setItem(CONFLICT_POLICY_KEY, "rename");
    await window.dbxPlugin.invoke("files/write", {
      path: "/docs/dup.txt",
      dataBase64: window.dbxPlugin.encodeBase64(new Uint8Array([65])), // "A"
      connectionId: "mock-conn",
    });
    mountWorkbench();
    await settle();
    await pickFolder([folderFile("docs/dup.txt", "B"), folderFile("docs/new.txt", "C")]);
    await settle();
    await settle();
    expect(wrapper!.find(".wb-dialog-backdrop").exists()).toBe(false);
    expect(await listNames("/docs")).toEqual(expect.arrayContaining(["dup.txt", "dup (1).txt", "new.txt"]));
  });
});
