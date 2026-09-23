// @vitest-environment happy-dom
// 粘贴上传（Finder/Explorer「复制文件 → Ctrl/Cmd+V」，对标 ssh 插件 sftp 面板）
// 与宿主级拖拽（fileTransfer.onDragState/onDrop）集成冒烟：剪贴板带文件才拦截
// 上传；纯文本粘贴放行；items 回退覆盖；只读拒绝；宿主句柄拖入走宿主桥读盘。
// 真实 App + mock 桥。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileToolbar from "./components/FileToolbar.vue";
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
  window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1");
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

/** 记录 files/upload/start 调用，其余方法照常走 mock 宿主。 */
function stubUploadStart() {
  const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
  const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
  vi.spyOn(window.dbxPlugin, "invoke").mockImplementation((async (
    method: string,
    params?: Record<string, unknown>,
    options?: { timeoutMs?: number },
  ) => {
    if (method === "files/upload/start") calls.push({ method, params: params ?? {} });
    return raw(method, params, options);
  }) as typeof window.dbxPlugin.invoke);
  return calls;
}

/** 构造 paste 事件：files/items 直塞 clipboardData（happy-dom 不实现剪贴板桥）。 */
function pasteEvent(data: { files?: File[]; items?: Array<{ kind: string; getAsFile: () => File | null }>; text?: string }) {
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", {
    value: {
      files: data.files ?? [],
      items: data.items ?? [],
      getData: (type: string) => (type === "text/plain" && data.text ? data.text : ""),
    },
  });
  return event;
}

interface HostFileTransferStub {
  pick: ReturnType<typeof vi.fn>;
  read: ReturnType<typeof vi.fn>;
  cancel: ReturnType<typeof vi.fn>;
  onDragState: ReturnType<typeof vi.fn>;
  onDrop: ReturnType<typeof vi.fn>;
  listeners: { drag: Array<(active: boolean) => void>; drop: Array<(files: Array<{ handleId: string; name: string; size: number; contentType: string }>) => void> };
}

/** mount 前注入宿主 fileTransfer 桩（mock 宿主不实现该桥），捕获订阅监听器。 */
function injectHostFileTransfer(): HostFileTransferStub {
  const stub: HostFileTransferStub = {
    pick: vi.fn(),
    read: vi.fn(async () => ({ dataBase64: window.dbxPlugin.encodeBase64(new Uint8Array([65])), length: 1, eof: true })),
    cancel: vi.fn(async () => undefined),
    onDragState: vi.fn(),
    onDrop: vi.fn(),
    listeners: { drag: [], drop: [] },
  };
  stub.onDragState.mockImplementation((listener: (active: boolean) => void) => {
    stub.listeners.drag.push(listener);
    return () => undefined;
  });
  stub.onDrop.mockImplementation((listener: (files: Array<{ handleId: string; name: string; size: number; contentType: string }>) => void) => {
    stub.listeners.drop.push(listener);
    return () => undefined;
  });
  Object.assign(window.dbxPlugin, { fileTransfer: { pick: stub.pick, read: stub.read, cancel: stub.cancel, onDragState: stub.onDragState, onDrop: stub.onDrop } });
  return stub;
}

describe("paste-to-upload", () => {
  it("uploads files pasted from the OS clipboard", async () => {
    mountWorkbench();
    await settle();
    const calls = stubUploadStart();
    document.dispatchEvent(pasteEvent({ files: [new File(["A"], "pasted.txt")] }));
    await settle();
    expect(calls).toHaveLength(1);
    expect(String(calls[0].params.remotePath)).toContain("pasted.txt");
  });

  it("lets plain-text paste through untouched", async () => {
    mountWorkbench();
    await settle();
    const calls = stubUploadStart();
    const event = pasteEvent({ text: "hello" });
    document.dispatchEvent(event);
    await settle();
    expect(calls).toEqual([]);
    expect(event.defaultPrevented).toBe(false);
  });

  it("falls back to DataTransferItem entries when files is empty", async () => {
    mountWorkbench();
    await settle();
    const calls = stubUploadStart();
    document.dispatchEvent(pasteEvent({ items: [{ kind: "file", getAsFile: () => new File(["A"], "item-paste.png") }] }));
    await settle();
    expect(calls).toHaveLength(1);
    expect(String(calls[0].params.remotePath)).toContain("item-paste.png");
  });

  it("refuses paste upload on a read-only connection", async () => {
    window.history.replaceState(null, "", "/?mock=1&locale=en&delay=0&local=1&ro=1");
    Reflect.deleteProperty(window, "dbxPlugin");
    installMockHost();
    mountWorkbench();
    await settle();
    const calls = stubUploadStart();
    document.dispatchEvent(pasteEvent({ files: [new File(["A"], "denied.txt")] }));
    await settle();
    expect(calls).toEqual([]);
  });

  it("advertises the paste shortcut on the upload button", async () => {
    mountWorkbench();
    await settle();
    const upload = wrapper!.getComponent(FileToolbar).findAll("button")
      .find((button) => button.attributes("aria-label") === workbenchMessage("en", "uploadPasteHint"))!;
    expect(upload).toBeTruthy();
  });
});

describe("host-level drag & drop (fileTransfer bridge)", () => {
  it("uploads host-dropped handle files through the host bridge", async () => {
    const stub = injectHostFileTransfer();
    mountWorkbench();
    await settle();
    const calls = stubUploadStart();
    expect(stub.listeners.drop).toHaveLength(1);
    stub.listeners.drop[0]([{ handleId: "h1", name: "host-drop.txt", size: 1, contentType: "text/plain" }]);
    await settle();
    expect(calls).toHaveLength(1);
    expect(String(calls[0].params.remotePath)).toContain("host-drop.txt");
    expect(stub.read).toHaveBeenCalledWith("h1", 0, expect.any(Number));
    // 句柄在读完后释放（uploadHostFiles finally）。
    expect(stub.cancel).toHaveBeenCalledWith("h1");
  });

  it("shows the host drag overlay while dragging and hides it after", async () => {
    const stub = injectHostFileTransfer();
    mountWorkbench();
    await settle();
    expect(wrapper!.find(".wb-host-drop-overlay").exists()).toBe(false);
    stub.listeners.drag[0](true);
    await nextTick();
    expect(wrapper!.find(".wb-host-drop-overlay").exists()).toBe(true);
    stub.listeners.drag[0](false);
    await nextTick();
    expect(wrapper!.find(".wb-host-drop-overlay").exists()).toBe(false);
  });

  it("does not subscribe when the host bridge is missing (web mode)", async () => {
    mountWorkbench();
    await settle();
    expect(wrapper!.find(".wb-host-drop-overlay").exists()).toBe(false);
    document.dispatchEvent(pasteEvent({ files: [new File(["A"], "web-paste.txt")] }));
    await settle();
    expect(wrapper!.find(".wb-host-drop-overlay").exists()).toBe(false);
  });
});
