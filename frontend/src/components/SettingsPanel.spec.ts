// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { config, mount } from "@vue/test-utils";
import SettingsPanel from "./SettingsPanel.vue";
import { vTip } from "../lib/tooltip";

config.global.directives = { tip: vTip };

const t = (key: string) => key;

function ensureHost() {
  window.dbxPlugin = (window.dbxPlugin ?? {}) as Window["dbxPlugin"];
}

function mountPanel(overrides: Partial<{ canSaveLocal: boolean; saveDir: string; defaultSaveDir: string }> = {}) {
  ensureHost();
  return mount(SettingsPanel, {
    props: {
      t,
      canSaveLocal: true,
      saveDir: "",
      defaultSaveDir: "/Users/me/Downloads",
      ...overrides,
    },
  });
}

afterEach(() => {
  vi.restoreAllMocks();
  if (window.dbxPlugin) Reflect.deleteProperty(window.dbxPlugin, "fileTransfer");
});

describe("SettingsPanel download directory", () => {
  it("renders the default path as a placeholder and emits a trimmed preference value", async () => {
    const wrapper = mountPanel();
    const input = wrapper.get("input");
    expect(input.attributes("placeholder")).toBe("/Users/me/Downloads");
    await input.setValue("/tmp/drop");
    expect(wrapper.emitted("save-dir")).toEqual([["/tmp/drop"]]);
  });

  it("restores the default directory by emitting an empty value", async () => {
    const wrapper = mountPanel({ saveDir: "/tmp/drop" });
    await wrapper.get("button[title='restoreDefaultDirectory']").trigger("click");
    expect(wrapper.emitted("save-dir")).toEqual([[""]]);
  });

  it("explains the fallback when local saving is unavailable", () => {
    const wrapper = mountPanel({ canSaveLocal: false });
    expect(wrapper.find("input").exists()).toBe(false);
    expect(wrapper.text()).toContain("downloadDirectoryUnavailable");
  });

  it("does not show a directory picker when the host does not expose one", () => {
    const wrapper = mountPanel();
    expect(wrapper.find("button[title='chooseDirectory']").exists()).toBe(false);
  });

  it("uses the optional host directory picker and saves its returned path", async () => {
    const pickDirectory = vi.fn().mockResolvedValue({ path: "/Volumes/Archive" });
    ensureHost();
    Object.defineProperty(window.dbxPlugin, "fileTransfer", { configurable: true, value: { pickDirectory } });
    const wrapper = mountPanel();
    await wrapper.get("button[title='chooseDirectory']").trigger("click");
    await Promise.resolve();
    expect(pickDirectory).toHaveBeenCalledOnce();
    expect(wrapper.emitted("save-dir")).toEqual([["/Volumes/Archive"]]);
  });

  it("does not change the value when the native picker is canceled", async () => {
    const pickDirectory = vi.fn().mockResolvedValue(undefined);
    ensureHost();
    Object.defineProperty(window.dbxPlugin, "fileTransfer", { configurable: true, value: { pickDirectory } });
    const wrapper = mountPanel({ saveDir: "/tmp/current" });
    await wrapper.get("button[title='chooseDirectory']").trigger("click");
    await Promise.resolve();
    expect(wrapper.emitted("save-dir")).toBeUndefined();
  });
});
