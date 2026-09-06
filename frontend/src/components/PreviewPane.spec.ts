// @vitest-environment happy-dom
import { afterEach, describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { bindApi } from "../lib/api";
import PreviewPane from "./PreviewPane.vue";

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
});
