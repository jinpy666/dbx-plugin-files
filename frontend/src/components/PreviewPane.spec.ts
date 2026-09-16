// @vitest-environment happy-dom
import { afterEach, describe, expect, it } from "vitest";
import { mount, config, type VueWrapper } from "@vue/test-utils";
import { bindApi } from "../lib/api";
import PreviewPane from "./PreviewPane.vue";
import { READ_MAX_BYTES } from "../lib/preview";
import { vTip } from "../lib/tooltip";

// 模板里的 v-tip（图标按钮提示）在测试挂载时同样需要指令注册。
config.global.directives = { tip: vTip };

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
    global: { stubs: { TextPreview: true } },
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
