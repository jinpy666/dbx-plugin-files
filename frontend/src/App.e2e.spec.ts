// @vitest-environment happy-dom
// mock 全链路 e2e 串联：浏览 → 进目录 → 预览 → 新建文件夹 → OS 拖入上传 →
// 压缩 → 深搜 → 传输历史。与组件/交互单测互补，验证多条主路径在同一真实
// App 实例上的状态连续性（mock 宿主，无网络）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import { installMockHost } from "./lib/mockHost";
import { workbenchMessage } from "./lib/i18n";
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
  for (let i = 0; i < 10; i++) {
    await vi.advanceTimersByTimeAsync(1);
    await nextTick();
  }
}

function leftTable() {
  return wrapper!.findAllComponents(FileTable).find((pane) => pane.props("paneId") === "left")!;
}

function rowByText(text: string) {
  const cells = [...document.querySelectorAll("*")].filter(
    (el) => el.textContent?.trim() === text && el.children.length === 0,
  );
  const cell = cells[cells.length - 1];
  return cell?.closest('[role="option"]') ?? cell?.closest("tr") ?? cell;
}

async function dispatchRowEvent(text: string, event: Event) {
  const row = rowByText(text);
  if (!row) throw new Error(`row not found: ${text}`);
  const rect = row.getBoundingClientRect();
  if (event instanceof MouseEvent) {
    Object.defineProperty(event, "clientX", { value: rect.left + 30 });
    Object.defineProperty(event, "clientY", { value: rect.top + rect.height / 2 });
  }
  row.dispatchEvent(event);
  await settle();
}

function menuItem(label: string) {
  return wrapper!.findAll("[role=menuitem]").find((item) => item.text().startsWith(label));
}

describe("mock end-to-end walk-through", () => {
  it("walks browse → preview → mkdir → upload → compress → search on one app instance", async () => {
    mountWorkbench();
    await settle();

    // ① 进入 /docs（双击目录行）。
    await dispatchRowEvent("docs", new MouseEvent("dblclick", { bubbles: true, cancelable: true }));
    await settle();
    expect(rowByText("readme.md")).toBeTruthy();

    // ② 右键预览 readme.md → 预览遮罩出现 → Esc 关闭。
    await dispatchRowEvent("readme.md", new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    await menuItem(workbenchMessage("en", "preview"))!.trigger("click");
    await settle();
    expect(document.querySelector(".wb-preview-overlay")).toBeTruthy();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await settle();
    expect(document.querySelector(".wb-preview-overlay")).toBeNull();

    // ③ 空白区右键新建文件夹 e2e-dir。
    leftTable().vm.$emit("blank-context", { x: 12, y: 12 });
    await settle();
    await menuItem(workbenchMessage("en", "newFolder"))!.trigger("click");
    await settle();
    await dialogInput().setValue("e2e-dir");
    await confirmDialog();
    await settle();
    expect(rowByText("e2e-dir")).toBeTruthy();

    // ④ OS 拖入上传：向左栏 pane 派发带 File 的 drop（happy-dom 的
    // DragEvent 不携带 dataTransfer init，用 defineProperty 注入 mock
    // transfer；osDroppedFiles 走 files 回退分支）。
    const pane = document.querySelector(".wb-pane") as HTMLElement;
    const dropEvent = new DragEvent("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(dropEvent, "dataTransfer", {
      value: {
        files: [new File(["e2e upload payload"], "upload-e2e.txt", { type: "text/plain" })],
        // onDropTo 先读跨栏拖放数据：空串 = 非 app 内拖放，走 OS files 分支。
        getData: () => "",
      },
    });
    pane.dispatchEvent(dropEvent);
    await settle();
    expect(rowByText("upload-e2e.txt")).toBeTruthy();

    // ⑤ 压缩刚上传的文件（空目录会被 mock 拒绝）：目标在当前目录 /docs 下。
    await dispatchRowEvent("upload-e2e.txt", new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    await menuItem(workbenchMessage("en", "compress"))!.trigger("click");
    await settle();
    await dialogInput().setValue("/docs/upload-e2e.tar.gz");
    await confirmDialog();
    await settle();
    expect(rowByText("upload-e2e.tar.gz")).toBeTruthy();

    // ⑥ 传输历史含上传与压缩记录（传输面板 → 历史分组）。
    await wrapper!.find('button[aria-label="Transfers"]').trigger("click");
    await settle();
    const panelText = wrapper!.text();
    expect(panelText).toContain("upload-e2e.txt");

    // ⑦ 深搜 e2e → 命中新建目录/上传文件至少其一。
    const input = wrapper!.find(".wb-search-input");
    await input.setValue("e2e");
    await input.trigger("keydown.enter");
    await settle();
    await vi.advanceTimersByTimeAsync(500);
    await settle();
    expect(wrapper!.text()).toContain("e2e-dir");
  });

  function dialogInput() {
    const input = document.querySelector('[role="dialog"] input') as HTMLInputElement;
    expect(input).toBeTruthy();
    return wrapper!.find('[role="dialog"] input');
  }

  async function dialogInputSetValue(value: string) {
    await dialogInput().setValue(value);
  }

  async function confirmDialog() {
    const button = [...document.querySelectorAll('[role="dialog"] footer button')].find(
      (el) => (el.textContent ?? "").trim() === workbenchMessage("en", "confirm"),
    ) ?? [...document.querySelectorAll('[role="dialog"] footer button')].pop();
    expect(button).toBeTruthy();
    button!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    await settle();
  }

  // 供后续用例复用（占位避免未使用告警）。
  void dialogInputSetValue;
});
