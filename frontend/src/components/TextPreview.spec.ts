// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import TextPreview from "./TextPreview.vue";

// CodeMirror 依赖 ResizeObserver 观察编辑器尺寸（happy-dom 无内置实现）。
class ResizeObserverStub {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}
if (!("ResizeObserver" in globalThis)) {
  (globalThis as unknown as { ResizeObserver: typeof ResizeObserverStub }).ResizeObserver = ResizeObserverStub;
}

// happy-dom 的 programmatic focus 不派发 focus 事件，CodeMirror 的
// `.cm-focused` 类不翻转；以 activeElement 是否落在编辑内容上判定焦点。
function editorHasFocus(wrapper: ReturnType<typeof mount>): boolean {
  const content = wrapper.find(".cm-content");
  return content.exists() && document.activeElement === content.element;
}

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

async function flush(times = 8) {
  for (let i = 0; i < times; i += 1) await Promise.resolve();
}

describe("TextPreview (P1-4 编辑器焦点)", () => {
  it("creates a read-only editor without stealing focus", async () => {
    const wrapper = mount(TextPreview, {
      props: { text: "hello", fileName: "notes.txt", appearance, editable: false },
      attachTo: document.body,
    });
    await flush();
    expect(wrapper.find(".cm-editor").exists()).toBe(true);
    expect(editorHasFocus(wrapper)).toBe(false);
    wrapper.unmount();
  });

  it("focuses the editor when mounted editable", async () => {
    const wrapper = mount(TextPreview, {
      props: { text: "hello", fileName: "notes.txt", appearance, editable: true },
      attachTo: document.body,
    });
    await flush();
    expect(editorHasFocus(wrapper)).toBe(true);
    wrapper.unmount();
  });

  it("exposes focus() for the parent pane", async () => {
    const wrapper = mount(TextPreview, {
      props: { text: "hello", fileName: "notes.txt", appearance, editable: false },
      attachTo: document.body,
    });
    await flush();
    (wrapper.vm as unknown as { focus(): void }).focus();
    await flush(2);
    expect(editorHasFocus(wrapper)).toBe(true);
    wrapper.unmount();
  });
});
