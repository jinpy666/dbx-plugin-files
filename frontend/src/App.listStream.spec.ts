// @vitest-environment happy-dom
// 流式目录列表（files/listStream P1）集成冒烟：chunk 分批渲染（到达顺序）→
// done 帧恢复排序、导航切换触发 files/listCancel 并丢弃迟到帧、失败帧保留
// 部分条目 + 「已加载 N 项后失败」横幅、ack 失败无缝回落 files/list、
// requestId 不匹配帧丢弃、done 条目达阈值仍提示大目录。真实 App + mock 桥。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import App from "./App.vue";
import FileTable from "./components/FileTable.vue";
import PathField from "./components/PathField.vue";
import { installMockHost } from "./lib/mockHost";
import { messages, workbenchMessage } from "./lib/i18n";
import { vTip } from "./lib/tooltip";
import { saveUiPrefs } from "./lib/prefs";

let wrapper: VueWrapper | undefined;
let mock: NonNullable<ReturnType<typeof installMockHost>>;

beforeEach(() => {
  vi.useFakeTimers();
  window.localStorage.clear();
  // 排序状态持久化走 size desc：与 mock 的到达顺序（名称升序）可区分，
  // 便于断言「流式期间按到达顺序、done 后恢复列排序」。
  saveUiPrefs({ sort: { column: "size", direction: "desc" }, leftSideTab: "quick", rightSideTab: "tree", leftSideCollapsed: false, rightSideCollapsed: false });
  Reflect.deleteProperty(window, "dbxPlugin");
});

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "dbxPlugin");
});

function mountWorkbench(query: string) {
  window.history.replaceState(null, "", `/?mock=1&locale=en&delay=0${query}`);
  mock = installMockHost() as NonNullable<ReturnType<typeof installMockHost>>;
  wrapper = mount(App, { attachTo: document.body, global: { directives: { tip: vTip } } });
  return wrapper;
}

/** 推进假定时器直至流式批次全部落地（覆盖 delay + interval 级联）。 */
async function flush(totalMs: number, step = 50) {
  for (let elapsed = 0; elapsed < totalMs; elapsed += step) {
    await vi.advanceTimersByTimeAsync(step);
    await nextTick();
  }
}

function leftTable() {
  return wrapper!.findAllComponents(FileTable)[0];
}

function leftEntries() {
  return leftTable().props("entries") as Array<{ path: string; name: string; size?: number }>;
}

async function navigate(target: string) {
  wrapper!.getComponent(PathField).vm.$emit("navigate", target);
  await nextTick();
}

describe("listStream streaming render (分批渲染与排序恢复)", () => {
  it("renders chunks in arrival order and restores the column sort on done", async () => {
    mountWorkbench("&streamBatch=2&streamInterval=50");
    await flush(400);
    // 初始目录 "/" 已通过 listStream 完成（11 项）。
    expect(leftEntries()).toHaveLength(11);

    await navigate("/docs");
    // 前两批（seq1/seq2）到达：4 项按到达顺序（mock 名称升序）展示，
    // 而非当前 size desc 排序——证明流式期间跳过了 sortEntries。
    await flush(100, 50);
    expect(leftTable().props("loading")).toBe(false);
    expect(leftEntries().map((item) => item.name)).toEqual(["data.bin", "data.csv", "data.json", "logo.png"]);

    // done 帧交付：13 项全量，恢复 size desc 排序（首行 = 体积最大条目）。
    await flush(400);
    const finalEntries = leftEntries();
    expect(finalEntries).toHaveLength(13);
    const largest = [...finalEntries].sort((a, b) => (b.size ?? 0) - (a.size ?? 0))[0]!;
    expect(finalEntries[0]!.name).toBe(largest.name);
    expect(leftTable().text()).toContain(workbenchMessage("en", "entriesCount", { count: 13 }));
  });
});

describe("listStream cancel on navigation (导航切换)", () => {
  it("cancels the in-flight session and drops its late frames", async () => {
    mountWorkbench("&streamBatch=2&streamInterval=50");
    await flush(400);
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    const methods: string[] = [];
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params?: unknown) => {
      methods.push(method);
      return raw<T>(method, params);
    });

    await navigate("/docs");
    await flush(100, 50);
    expect(leftEntries()).toHaveLength(4);
    // 切换目录：上一会话 best-effort files/listCancel（契约：不 await）。
    await navigate("/empty");
    await flush(500);
    expect(methods).toContain("files/listCancel");
    // 旧 /docs 会话的迟到帧不得落地：最终停留在新目录（空目录）。
    expect(leftEntries()).toHaveLength(0);
    expect(wrapper!.find(".wb-file-empty").text()).toBe(workbenchMessage("en", "emptyDirectory"));
  });
});

describe("listStream failure frame (失败帧保留部分数据)", () => {
  it("keeps loaded entries and shows the partialListFailed banner", async () => {
    mountWorkbench("&streamBatch=2&streamInterval=0&streamFailAfter=1");
    await flush(400);
    // 初始导航在首批（2 项）后收到失败帧：条目保留（状态层）+ listingFailed 错误横幅。
    expect(leftTable().props("failed")).toBe(true);
    expect(leftEntries()).toHaveLength(2);
    const banner = wrapper!.get(".wb-error-banner");
    expect(banner.text()).toBe(workbenchMessage("en", "partialListFailed", { count: 2 }));
    // FileTable 既有 failed 语义（失败占位卡替代行区，freshReview 套件锚定）
    // 保持不变：失败态展示重试入口，部分条目保留在列表状态里等待重试交付。
    expect(wrapper!.get(".wb-file-empty").text()).toContain(workbenchMessage("en", "directoryLoadFailed"));
  });
});

describe("listStream ack fallback (ack 失败无缝回落)", () => {
  it("falls back to files/list when the ack reports disabled", async () => {
    mountWorkbench("&streamDisabled=1");
    const raw = window.dbxPlugin.invoke.bind(window.dbxPlugin);
    const methods: string[] = [];
    vi.spyOn(window.dbxPlugin, "invoke").mockImplementation(async <T,>(method: string, params?: unknown) => {
      methods.push(method);
      return raw<T>(method, params);
    });
    await flush(400);
    // 先试 listStream（ack 报 disabled）再回落 files/list，UI 无感（全量 11 项）。
    expect(methods).toContain("files/listStream");
    expect(methods).toContain("files/list");
    expect(leftTable().props("failed")).toBe(false);
    expect(leftEntries()).toHaveLength(11);
    expect(leftTable().text()).toContain(workbenchMessage("en", "entriesCount", { count: 11 }));
  });
});

describe("listStream event routing (requestId 不匹配丢弃)", () => {
  it("ignores chunk events for unknown request ids", async () => {
    mountWorkbench("&streamBatch=2&streamInterval=0");
    await flush(400);
    expect(leftEntries()).toHaveLength(11);
    mock.emitEvent("files/list/chunk", {
      requestId: "forged-request",
      seq: 1,
      entries: [{ name: "spoof.txt", path: "/spoof.txt", kind: "file", size: 1, modifiedAt: "" }],
      done: true,
    });
    await nextTick();
    // 伪造/未知 requestId 的帧被路由层直接丢弃，列表不受影响。
    expect(leftEntries()).toHaveLength(11);
    expect(wrapper!.find(".wb-error-banner").exists()).toBe(false);
  });
});

describe("listStream large directory notice (done 帧大目录提示)", () => {
  it("keeps the largeDirectory notice for streamed done frames at threshold", async () => {
    mountWorkbench("&streamBatch=4000&streamInterval=0");
    await flush(400);
    await navigate("/10k");
    await flush(400, 50);
    expect(leftEntries()).toHaveLength(10_000);
    expect(wrapper!.get(".wb-notice").text()).toBe(workbenchMessage("en", "largeDirectory", { count: 10_000 }));
    expect(leftTable().text()).toContain(workbenchMessage("en", "entriesCount", { count: 10_000 }));
  });
});

// i18n 七语完整性（partialListFailed 与占位符名一致由 i18n.spec 全量校验兜底）。
describe("listStream i18n key", () => {
  it("defines partialListFailed for all seven locales", () => {
    for (const locale of Object.keys(messages) as Array<keyof typeof messages>) {
      expect(workbenchMessage(locale, "partialListFailed", { count: 3 })).toContain("3");
    }
  });
});

describe("listStream events-before-ack race (预缓冲重放)", () => {
  it("replays chunks that arrived before the ack (real-bridge write-order race)", async () => {
    // 复现真实宿主桥竞态：第一批（含快路径 done）在 ack 应答前同步写入。
    // 无预缓冲时该帧按「requestId 未登记」被丢弃，目录永远加载不出来。
    mountWorkbench("&streamEventsBeforeAck=1&streamBatch=4&streamInterval=50");
    await flush(400);
    expect(leftEntries()).toHaveLength(11);
    expect(leftTable().props("loading")).toBe(false);
  });
});
