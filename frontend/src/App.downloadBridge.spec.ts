// @vitest-environment happy-dom
// 下载落盘兜底策略（对标 ssh 插件 issue #93）：沙箱 workbench iframe 内程序化
// <a download> 会被浏览器静默丢弃（无报错、无下载事件）——「提示下载成功但
// 本机没有文件」。唯一可靠兜底是宿主 host.saveFile 桥（Host API 1.1，顶层
// 文档不受 sandbox 约束）。本文件用结构断言钉死「不许回退 iframe anchor」与
// beginSave 取消契约，并经 mock 宿主跑通无 fileTransfer 的下载链路。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import appVueSource from "./App.vue?raw";
import FileTable from "./components/FileTable.vue";
import FileToolbar from "./components/FileToolbar.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import type { FileEntry } from "./lib/api";

// App.vue 源码反向锁定约束（ssh workbench.spec 同款防线）。
const appScript = appVueSource.slice(appVueSource.indexOf("<script"), appVueSource.indexOf("</script>"));

let wrapper: VueWrapper | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  Reflect.deleteProperty(window, "dbxPlugin");
  // 不带 &local=1：模拟 web 宿主（canSaveLocal=false、无 fileTransfer），
  // 下载必须走 host.saveFile 兜底。
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

async function settle() {
  for (let i = 0; i < 8; i++) {
    await vi.advanceTimersByTimeAsync(1);
    await nextTick();
  }
}

function seedRemoteFile(name: string, bytes: Uint8Array) {
  return window.dbxPlugin.invoke("files/write", {
    path: `/${name}`,
    dataBase64: window.dbxPlugin.encodeBase64(bytes),
    connectionId: "mock-conn",
  });
}

describe("issue #93 download fallback policy (structural guards)", () => {
  it("never creates in-iframe anchor downloads in any save path", () => {
    // 历史函数 saveBrowserDownload 的 blob+anchor 在 sandbox="allow-scripts"
    // 下被浏览器静默丢弃，且报成功——不允许以任何形式回归。
    expect(appScript).not.toContain("saveBrowserDownload");
    expect(appScript).not.toContain("anchor.download");
    expect(appScript).not.toContain('createElement("a")');
  });

  it("routes the no-fileTransfer fallback through the host saveFile bridge", () => {
    const start = appScript.indexOf("async function saveHostFile(");
    expect(start).toBeGreaterThanOrEqual(0);
    const body = appScript.slice(start, appScript.indexOf("\n}", start));
    expect(body).toContain("window.dbxPlugin.saveFile");
    expect(body).toContain("localSaveUnavailable");
    expect(body).toContain("localSaveTooLarge");
    expect(body).toContain("if (!saved)");
  });

  it("aborts the whole download when beginSave resolves null (user cancel)", () => {
    // beginSave 契约：用户取消原生保存框返回 null。只判 truthy 会让
    // target=null 滑进兜底分支并最终提示成功——必须取消整次下载。
    const begin = appScript.indexOf("await hostTransfer.beginSave(");
    expect(begin).toBeGreaterThanOrEqual(0);
    const context = appScript.slice(begin, begin + 400);
    expect(context).toContain("?? undefined");
    expect(context).toContain("files/transfer/cancel");
    expect(context).toContain("throw new TransferCanceled()");
  });
});

describe("no-fileTransfer download via host saveFile (integration)", () => {
  it("delivers the full payload to host.saveFile and reports success", async () => {
    await seedRemoteFile("bridge.txt", new Uint8Array([65, 66, 67]));
    const saveFile = vi.spyOn(window.dbxPlugin, "saveFile");
    wrapper = mount(App, { attachTo: document.body, global: { directives: { tip: vTip } } });
    await settle();
    const table = wrapper.findAllComponents(FileTable)[0]!;
    const entry = (table.props("entries") as FileEntry[]).find((candidate) => candidate.name === "bridge.txt");
    expect(entry, "remote fixture bridge.txt should be listed").toBeTruthy();
    table.vm.$emit("update:selection", [entry!.path]);
    await settle();
    const download = wrapper.getComponent(FileToolbar).findAll("button")
      .find((button) => button.attributes("aria-label") === workbenchMessage("en", "download"))!;
    await download.trigger("click");
    // start → 帧泵 → finish → saveHostFile 的多段异步链路。
    await settle();
    await settle();
    await settle();
    await settle();
    expect(saveFile).toHaveBeenCalledTimes(1);
    const [options, data] = saveFile.mock.calls[0]!;
    expect(options.fileName).toBe("bridge.txt");
    // mock 下载泵推的是合成帧（(offset/7)%251 填充），不回传源内容——
    // 这里断言整包长度正确即可；内容正确性由 sidecar 帧协议保证。
    expect((data as Uint8Array).byteLength).toBe(3);
    expect(wrapper.find(".wb-error-banner").exists()).toBe(false);
    expect(wrapper.find(".wb-notice").text())
      .toBe(workbenchMessage("en", "downloaded", { name: "bridge.txt" }));
  });
});
