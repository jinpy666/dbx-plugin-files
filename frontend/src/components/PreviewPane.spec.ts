// @vitest-environment happy-dom
import { afterEach, describe, expect, it } from "vitest";
import { mount, config, type VueWrapper } from "@vue/test-utils";
import { defineComponent } from "vue";
import { bindApi } from "../lib/api";
import PreviewPane from "./PreviewPane.vue";
import { PREVIEW_MAX_BYTES, READ_MAX_BYTES } from "../lib/preview";
import { vTip } from "../lib/tooltip";

// 模板里的 v-tip（图标按钮提示）在测试挂载时同样需要指令注册。
config.global.directives = { tip: vTip };
config.global.stubs = { FileViewerPreview: true };

// 审计#7：替代 CodeMirror 的 TextPreview 桩——点击按钮即模拟草稿改动
// （emit change），expose focus 供编辑态聚焦链路调用。
const TextPreviewStub = defineComponent({
  props: { text: { type: String, default: "" }, editable: Boolean },
  emits: ["change"],
  setup(_props, { expose }) {
    expose({ focus: () => undefined });
  },
  template: `<div class="stub-text"><button data-test="type" @click="$emit('change', text + '!')" /></div>`,
});

const appearance: DbxPluginAppearance = {
  colorScheme: "dark",
  colors: {
    background: "#131416",
    foreground: "#d7d7db",
    muted: "#2a2a2d",
    mutedForeground: "#97989d",
    accent: "#2e2f33",
    accentForeground: "#dddde2",
    border: "#3a3a3d",
    destructive: "#ef4444",
  },
  terminal: { fontFamily: "monospace", fontSize: 12 },
};

function b64decode(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function b64encode(value: Uint8Array): string {
  let binary = "";
  for (const byte of value) binary += String.fromCharCode(byte);
  return btoa(binary);
}

// 1x1 png
const PNG_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

async function flush(times = 6) {
  for (let i = 0; i < times; i += 1) await Promise.resolve();
}

let wrapper: VueWrapper | undefined;

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
});

describe("PreviewPane (P1-4 预览焦点)", () => {
  it("moves focus into the preview container after open", async () => {
    bindApi(async <T,>() => ({ dataBase64: PNG_B64, truncated: false, size: 70 }) as unknown as T, null);
    window.dbxPlugin = {
      decodeBase64: b64decode,
      encodeBase64: (value) => btoa(String.fromCharCode(...(value instanceof Uint8Array ? value : new Uint8Array(value)))),
    } as DbxPluginApi;
    wrapper = mount(PreviewPane, {
      props: { path: "/docs/logo.png", canWrite: true, appearance, t: (key: string) => key },
      attachTo: document.body,
    });
    await flush();
    const preview = wrapper.find(".wb-preview");
    expect(preview.exists()).toBe(true);
    expect(document.activeElement).toBe(preview.element);
  });

  it("does not render when no path is given", () => {
    wrapper = mount(PreviewPane, {
      props: { path: null, canWrite: true, appearance, t: (key: string) => key },
    });
    expect(wrapper.find(".wb-preview").exists()).toBe(false);
  });

  // R5-P2-6：关闭（卸载）后焦点归还打开前的触发元素，不再落在 BODY
  it("returns focus to the triggering element after close", async () => {
    bindApi(async <T,>() => ({ dataBase64: PNG_B64, truncated: false, size: 70 }) as unknown as T, null);
    window.dbxPlugin = {
      decodeBase64: b64decode,
      encodeBase64: (value) => btoa(String.fromCharCode(...(value instanceof Uint8Array ? value : new Uint8Array(value)))),
    } as DbxPluginApi;
    wrapper = mount(
      {
        components: { PreviewPane },
        data: () => ({ path: null as string | null, appearance, t: (key: string) => key }),
        template: `
          <div>
            <button data-test="trigger">触发</button>
            <!-- 镜像 App.vue 真实挂载：外层 v-if 控制组件整体挂载/卸载 -->
            <PreviewPane v-if="path" :path="path" :can-write="false" :appearance="appearance" :t="t" />
          </div>
        `,
      },
      { attachTo: document.body },
    );
    const trigger = wrapper.find("[data-test=trigger]").element as HTMLElement;
    trigger.focus();
    expect(document.activeElement).toBe(trigger);
    await wrapper.setData({ path: "/docs/logo.png" });
    await flush();
    const preview = wrapper.find(".wb-preview");
    expect(preview.exists()).toBe(true);
    expect(document.activeElement).toBe(preview.element);
    await wrapper.setData({ path: null });
    await flush();
    expect(wrapper.find(".wb-preview").exists()).toBe(false);
    expect(document.activeElement).toBe(trigger);
  });
});

it("announces bounded preview loading without stale size or truncation feedback", async () => {
  let resolveRead!: (value: { dataBase64: string; truncated: boolean; size: number }) => void;
  const reads: Record<string, unknown>[] = [];
  bindApi(async <T,>(_method: string, params: unknown) => {
    reads.push(params as Record<string, unknown>);
    return new Promise<T>((resolve) => { resolveRead = (value) => resolve(value as T); });
  }, null);
  window.dbxPlugin = { decodeBase64: b64decode } as DbxPluginApi;
  wrapper = mount(PreviewPane, {
    props: { path: "/large.txt", canWrite: true, appearance, t: (key: string) => key },
    global: { stubs: { TextPreview: true, FileViewerPreview: true } },
  });
  expect(wrapper.get('[role=status]').text()).toBe('loading');
  expect(wrapper.get('.wb-preview-body').attributes('aria-busy')).toBe('true');
  expect(wrapper.get('.wb-preview-header').text()).not.toContain('0 B');
  expect(reads[0]).toMatchObject({ path: '/large.txt', maxBytes: READ_MAX_BYTES });
  resolveRead({ dataBase64: 'aGk=', truncated: true, size: READ_MAX_BYTES + 100 });
  await flush();
  expect(wrapper.find('.wb-notice').exists()).toBe(true);
  await wrapper.setProps({ path: '/next.txt' });
  expect(wrapper.get('[role=status]').text()).toBe('loading');
  expect(wrapper.find('.wb-notice').exists()).toBe(false);
  expect(wrapper.get('.wb-preview-header').text()).not.toContain('MiB');
  resolveRead({ dataBase64: 'aGk=', truncated: false, size: 2 });
  await flush();
  expect(wrapper.get('.wb-preview-body').attributes('aria-busy')).toBe('false');
});

// 审计#7：向外暴露 isDirty（editing 且草稿已改动），App 侧关闭预览前据此
// 弹丢弃确认，防止静默丢失 CodeMirror 编辑内容。
it("exposes unsaved-draft state as isDirty for the close guard", async () => {
  bindApi(async <T,>() => ({ dataBase64: "aGVsbG8=", truncated: false, size: 5 } as unknown as T), null);
  window.dbxPlugin = { decodeBase64: b64decode } as DbxPluginApi;
  wrapper = mount(PreviewPane, {
    props: { path: "/notes.txt", canWrite: true, appearance, t: (key: string) => key },
    global: { stubs: { TextPreview: TextPreviewStub } },
  });
  await flush();
  const dirty = () => (wrapper!.findComponent(PreviewPane).vm as unknown as { isDirty: boolean }).isDirty;
  expect(dirty()).toBe(false);
  // 进入编辑（铅笔为首按钮）但草稿未改动：仍不算脏。
  await wrapper.get(".wb-preview-header button").trigger("click");
  await flush();
  expect(dirty()).toBe(false);
  // 草稿改动 → isDirty。
  await wrapper.get(".stub-text [data-test=type]").trigger("click");
  await flush();
  expect(dirty()).toBe(true);
  // 取消编辑回滚草稿 → isDirty 复位。
  await wrapper.findAll(".wb-preview-editbar button")[0]!.trigger("click");
  await flush();
  expect(dirty()).toBe(false);
});

// 两套方案：文本走 CodeMirror，Office/PDF 走 FileViewer，未知二进制回退 hex。
it("routes strategies: text to CodeMirror, PDF to viewer, unknown binary to hex", async () => {
  bindApi(async <T,>(method: string) => {
    if (method === "files/archiveList") return { entries: [], total: 0 } as unknown as T;
    return { dataBase64: b64encode(new TextEncoder().encode("hello")), truncated: false, size: 5 } as unknown as T;
  }, null);
  window.dbxPlugin = { decodeBase64: b64decode } as DbxPluginApi;

  function b64encode(value: Uint8Array): string {
    let binary = "";
    for (const byte of value) binary += String.fromCharCode(byte);
    return btoa(binary);
  }

  wrapper = mount(PreviewPane, {
    props: { path: "/notes.txt", canWrite: true, appearance, t: (key: string) => key },
  });
  await flush();
  expect(wrapper.find(".preview-editor").exists()).toBe(true);

  await wrapper.setProps({ path: "/docs/report.pdf" });
  await flush();
  expect(wrapper.findComponent({ name: "FileViewerPreview" }).exists()).toBe(true);

  // 不可打印占比高的未知扩展 → hex dump
  const binary = new Uint8Array(64).fill(0x00);
  bindApi(async <T,>() => ({ dataBase64: b64encode(binary), truncated: false, size: 64 }) as unknown as T, null);
  await wrapper.setProps({ path: "/downloads/blob.weird" });
  await flush();
  expect(wrapper.find("pre.wb-hex").exists()).toBe(true);
});

// 平台外部打开引导（open-with per OS）：viewer 渲染不了的文档格式（office/pdf/
// media）在错误面板给出「配置外部应用」入口，跳设置弹窗 openWith 分类。
describe("PreviewPane external-open guidance", () => {
  it("offers the open-with settings shortcut when an office file fails to preview", async () => {
    bindApi(async <T,>() => Promise.reject(new Error("viewer renderer missing")) as unknown as T, null);
    window.dbxPlugin = {
      decodeBase64: b64decode,
      encodeBase64: (value) => btoa(String.fromCharCode(...(value instanceof Uint8Array ? value : new Uint8Array(value)))),
    } as DbxPluginApi;
    wrapper = mount(PreviewPane, {
      props: { path: "/docs/deck.pptx", canWrite: true, appearance, t: (key: string) => key },
      attachTo: document.body,
    });
    await flush();
    const notice = wrapper.find(".wb-preview-notice");
    expect(notice.exists()).toBe(true);
    expect(notice.text()).toContain("previewExternalHint");
    const settingsButton = notice.findAll("button").find((button) => button.text() === "openInSettings");
    expect(settingsButton).toBeTruthy();
    await settingsButton!.trigger("click");
    expect(wrapper.emitted("open-settings")).toHaveLength(1);
  });

  it("does not offer the shortcut for plain text failures", async () => {
    bindApi(async <T,>() => Promise.reject(new Error("no such file")) as unknown as T, null);
    window.dbxPlugin = {
      decodeBase64: b64decode,
      encodeBase64: (value) => btoa(String.fromCharCode(...(value instanceof Uint8Array ? value : new Uint8Array(value)))),
    } as DbxPluginApi;
    wrapper = mount(PreviewPane, {
      props: { path: "/docs/readme.md", canWrite: true, appearance, t: (key: string) => key },
      attachTo: document.body,
    });
    await flush();
    const notice = wrapper.find(".wb-preview-notice");
    expect(notice.exists()).toBe(true);
    expect(notice.text()).not.toContain("previewExternalHint");
    expect(wrapper.emitted("open-settings")).toBeUndefined();
  });
});

// ---- 分块流式加载（files/stat + files/readRange，parity-tools）----------------
// 二进制预览（图片/媒体/office/pdf）：2MiB 顺序分块拼装 Blob、确定性进度条、
// 取消干净关闭、>256MiB 超上限保持「下载代替」。
describe("PreviewPane chunked loading (files/readRange)", () => {
  interface CallLog {
    method: string;
    params: Record<string, unknown>;
  }

  function bindRangeApi(options: { total: number; gate?: (offset: number) => Promise<void> }) {
    const calls: CallLog[] = [];
    bindApi(async <T,>(method: string, params: unknown) => {
      const p = (params ?? {}) as Record<string, unknown>;
      calls.push({ method, params: p });
      if (method === "files/stat") {
        return { entry: { name: "big.pdf", path: "/docs/big.pdf", kind: "file", size: options.total } } as unknown as T;
      }
      if (method === "files/readRange") {
        const offset = Number(p.offset ?? 0);
        if (options.gate) await options.gate(offset);
        const length = Math.min(READ_MAX_BYTES, options.total - offset);
        const bytes = new Uint8Array(Math.max(0, length));
        bytes.fill((offset % 250) + 1);
        return {
          dataBase64: b64encode(bytes),
          totalSize: options.total,
          offset,
          eof: offset + length >= options.total,
        } as unknown as T;
      }
      throw new Error(`unexpected method ${method}`);
    }, null);
    return calls;
  }

  function mountPane(tOverride?: (key: string, values?: Record<string, string | number>) => string) {
    window.dbxPlugin = { decodeBase64: b64decode } as DbxPluginApi;
    wrapper = mount(PreviewPane, {
      props: {
        path: "/docs/big.pdf",
        canWrite: true,
        appearance,
        t: tOverride ?? ((key: string) => key),
      },
      attachTo: document.body,
    });
    return wrapper;
  }

  it("assembles readRange chunks into a viewer File without files/read", async () => {
    const total = 2 * READ_MAX_BYTES + 100;
    const calls = bindRangeApi({ total });
    const pane = mountPane();
    await flush(24);
    const viewer = pane.findComponent({ name: "FileViewerPreview" });
    expect(viewer.exists()).toBe(true);
    expect((viewer.props("source") as File).size).toBe(total);
    // 顺序（并发 1）分块：offset 单调推进；整读路径不再参与。
    expect(calls.filter((item) => item.method === "files/readRange").map((item) => item.params.offset)).toEqual([
      0,
      READ_MAX_BYTES,
      2 * READ_MAX_BYTES,
    ]);
    expect(calls.some((item) => item.method === "files/read")).toBe(false);
  });

  it("shows determinate byte progress while chunks arrive", async () => {
    const total = 2 * READ_MAX_BYTES;
    let releaseGate!: () => void;
    const gate = new Promise<void>((resolve) => {
      releaseGate = resolve;
    });
    const gated = new Set<number>();
    bindRangeApi({
      total,
      gate: async (offset) => {
        if (offset === 0) return;
        gated.add(offset);
        await gate;
      },
    });
    const pane = mountPane((key, values) => (values?.done !== undefined ? `${key} ${values.done}` : key));
    await flush(24);
    // 第一片（2MiB）已取，第二片被闸门挂起：进度条呈现 2MiB / 4MiB。
    const chunkbar = pane.get("[data-test=chunkbar]");
    expect(chunkbar.text()).toContain("previewChunkProgress 2.0 MiB");
    expect(chunkbar.get('[role=progressbar]').attributes("aria-valuenow")).toBe("50");
    releaseGate();
    await flush(24);
    expect(pane.find("[data-test=chunkbar]").exists()).toBe(false);
  });

  it("cancels the remaining chunks and closes cleanly", async () => {
    const total = 3 * READ_MAX_BYTES;
    let releaseGate!: () => void;
    const gate = new Promise<void>((resolve) => {
      releaseGate = resolve;
    });
    const calls = bindRangeApi({
      total,
      gate: async (offset) => {
        if (offset === READ_MAX_BYTES) await gate;
      },
    });
    const pane = mountPane();
    await flush(24);
    const rangesBefore = calls.filter((item) => item.method === "files/readRange").length;
    await pane.get("[data-test=chunk-cancel]").trigger("click");
    expect(pane.emitted("close")).toHaveLength(1);
    // 释放被挂起的第二片后不再有后续分片请求（取消在分片间隙打断）。
    releaseGate();
    await flush(24);
    expect(calls.filter((item) => item.method === "files/readRange").length).toBe(rangesBefore);
  });

  it("keeps the download-instead hint above the hard cap", async () => {
    bindRangeApi({ total: PREVIEW_MAX_BYTES + 1 });
    const pane = mountPane();
    await flush(24);
    const overCap = pane.get("[data-test=overcap]");
    expect(overCap.text()).toContain("previewOverCap");
    expect(pane.findComponent({ name: "FileViewerPreview" }).exists()).toBe(false);
    await overCap.get("button").trigger("click");
    expect(pane.emitted("download")).toEqual([["/docs/big.pdf"]]);
  });
});

// ---- Markdown 渲染视图（MarkdownRender.vue + 头部「渲染/源码」切换）----------
// md 默认进渲染面（.wb-md-render）；切换回 CodeMirror 源码（.preview-editor）；
// <script> 内容不落 DOM；解析异常回退源码由组件 emit render-error 驱动。
describe("PreviewPane markdown render view", () => {
  const MD_TEXT = ["# Readme", "", "hello <script>alert(1)</script> world", "", "- alpha", "- beta"].join("\n");

  function mountMd(path = "/docs/README.md") {
    bindApi(async <T,>() => ({
      dataBase64: b64encode(new TextEncoder().encode(MD_TEXT)),
      truncated: false,
      size: MD_TEXT.length,
    } as unknown as T), null);
    window.dbxPlugin = { decodeBase64: b64decode } as DbxPluginApi;
    wrapper = mount(PreviewPane, {
      props: { path, canWrite: true, appearance, t: (key: string) => key },
      attachTo: document.body,
    });
  }

  function b64encode(value: Uint8Array): string {
    let binary = "";
    for (const byte of value) binary += String.fromCharCode(byte);
    return btoa(binary);
  }

  it("opens .md in the rendered view by default with the toggle present", async () => {
    mountMd();
    await flush();
    expect(wrapper!.find("[data-test=md-toggle]").exists()).toBe(true);
    const rendered = wrapper!.get(".wb-md-render");
    expect(rendered.get("h1").text()).toBe("Readme");
    expect(rendered.find("script").exists()).toBe(false);
    // 渲染态不再挂 CodeMirror 源码容器。
    expect(rendered.find(".preview-editor").exists()).toBe(false);
  });

  it("switches to the CodeMirror source view and back to the rendered view", async () => {
    mountMd();
    await flush();
    await wrapper!.get("[data-test=md-source]").trigger("click");
    await flush();
    expect(wrapper!.find(".wb-md-render").exists()).toBe(false);
    expect(wrapper!.find(".preview-editor").exists()).toBe(true);
    await wrapper!.get("[data-test=md-render]").trigger("click");
    await flush();
    expect(wrapper!.find(".wb-md-render").exists()).toBe(true);
    // 会话记忆复位为渲染态，避免污染同文件后续用例的默认视图。
    await wrapper!.get("[data-test=md-source]").trigger("click");
    await wrapper!.get("[data-test=md-render]").trigger("click");
    await flush();
  });

  it("hides the toggle for non-markdown text files", async () => {
    bindApi(async <T,>() => ({
      dataBase64: b64encode(new TextEncoder().encode("plain notes")),
      truncated: false,
      size: 11,
    } as unknown as T), null);
    window.dbxPlugin = { decodeBase64: b64decode } as DbxPluginApi;
    wrapper = mount(PreviewPane, {
      props: { path: "/notes.txt", canWrite: true, appearance, t: (key: string) => key },
    });
    await flush();
    expect(wrapper.find("[data-test=md-toggle]").exists()).toBe(false);
    expect(wrapper.find(".preview-editor").exists()).toBe(true);
  });
});
