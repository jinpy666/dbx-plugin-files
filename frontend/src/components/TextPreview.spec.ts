// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import type { Extension } from "@codemirror/state";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { tags } from "@lezer/highlight";
import TextPreview from "./TextPreview.vue";
// shared/frontend 公共层薄断言：本插件工具链下 editorTheme 的 import 解析、
// 真实 CodeMirror 运行时构造与调色板取值成立（接线方式见 TextPreview.vue）。
import { dbxSyntaxHighlight, EDITOR_TOKEN_COLORS, syntaxTokenSpecs } from "../../../shared/frontend/editorTheme";

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

describe("TextPreview 语法高亮注入（shared editorTheme）", () => {
  it("builds a highlight extension via the real CodeMirror runtime with brightened dark tokens", () => {
    // 与组件 extensions() 相同的装配：真实 HighlightStyle/syntaxHighlighting/tags
    // 构造成功即证明 shared 模块在 files 打包链路下可用。
    const extension = dbxSyntaxHighlight("dark", { HighlightStyle, syntaxHighlighting, tags }) as Extension;
    expect(extension).toBeTruthy();
    // 记忆化：同色系复用同一扩展实例
    expect(dbxSyntaxHighlight("dark", { HighlightStyle, syntaxHighlighting, tags })).toBe(extension);
    // 暗色关键字为提亮后的取值（覆盖 basicSetup 内置浅底 defaultHighlightStyle）
    expect(EDITOR_TOKEN_COLORS.dark.key).toBe("#4fc1ff");
    expect(syntaxTokenSpecs("dark").some((spec) => spec.tag === "keyword" && spec.color === "#4fc1ff")).toBe(true);
  });
});

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
