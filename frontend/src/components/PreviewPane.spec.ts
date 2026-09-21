// @vitest-environment happy-dom
import { afterEach, describe, expect, it } from "vitest";
import { mount, config, type VueWrapper } from "@vue/test-utils";
import { defineComponent } from "vue";
import { bindApi } from "../lib/api";
import PreviewPane from "./PreviewPane.vue";
import { READ_MAX_BYTES } from "../lib/preview";
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
