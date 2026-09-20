// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { config, mount } from "@vue/test-utils";
import SettingsPanel from "./SettingsPanel.vue";
import type { OpenAppPrefs } from "../lib/prefs";
import { vTip } from "../lib/tooltip";

config.global.directives = { tip: vTip };

const t = (key: string) => key;

function ensureHost() {
  window.dbxPlugin = (window.dbxPlugin ?? {}) as Window["dbxPlugin"];
}

function mountPanel(overrides: Partial<{ canSaveLocal: boolean; saveDir: string; defaultSaveDir: string; downloadDirError: string; openApp: OpenAppPrefs; openAppError: string }> = {}) {
  ensureHost();
  return mount(SettingsPanel, {
    props: {
      t,
      canSaveLocal: true,
      saveDir: "",
      defaultSaveDir: "/Users/me/Downloads",
      openApp: { defaultApp: "", mappings: [] },
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

  it("shows a validation error for an invalid download directory", () => {
    const wrapper = mountPanel({ downloadDirError: "Choose an existing absolute directory." });
    expect(wrapper.get("input").attributes("aria-invalid")).toBe("true");
    expect(wrapper.get("[role='alert']").text()).toContain("existing absolute directory");
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

describe("SettingsPanel external open-with app (issue #11)", () => {
  it("emits the typed default app while keeping the mapping rows untouched", async () => {
    const wrapper = mountPanel({
      openApp: { defaultApp: "/old/app", mappings: [{ ext: "ini", app: "/apps/np" }] },
    });
    const appInput = wrapper.get("input[aria-label='externalApp']");
    expect((appInput.element as HTMLInputElement).value).toBe("/old/app");
    await appInput.setValue("/apps/notepad++");
    expect(wrapper.emitted("save-open-app")).toEqual([
      [{ defaultApp: "/apps/notepad++", mappings: [{ ext: "ini", app: "/apps/np" }] }],
    ]);
  });

  it("restores the system default app by emitting an empty default", async () => {
    const wrapper = mountPanel({ openApp: { defaultApp: "/old/app", mappings: [] } });
    await wrapper.get("button[title='useSystemDefaultApp']").trigger("click");
    expect(wrapper.emitted("save-open-app")).toEqual([[{ defaultApp: "", mappings: [] }]]);
  });

  it("adds and edits mapping rows, emitting raw drafts for the parent to sanitize", async () => {
    const wrapper = mountPanel();
    await wrapper.get("button.wb-link-button").trigger("click");
    // A fresh row starts empty and is emitted as-is; the parent filters it.
    expect(wrapper.emitted("save-open-app")?.at(-1)).toEqual([
      { defaultApp: "", mappings: [{ ext: "", app: "" }] },
    ]);
    const extInput = wrapper.get("input[aria-label='extensionColumn']");
    const appInput = wrapper.get("input[aria-label='appColumn']");
    await extInput.setValue("ini");
    await appInput.setValue("/apps/np");
    expect(wrapper.emitted("save-open-app")?.at(-1)).toEqual([
      { defaultApp: "", mappings: [{ ext: "ini", app: "/apps/np" }] },
    ]);
  });

  it("removes a mapping row and emits the remaining rows", async () => {
    const wrapper = mountPanel({
      openApp: {
        defaultApp: "",
        mappings: [
          { ext: "ini", app: "/apps/np" },
          { ext: "conf", app: "/apps/vim" },
        ],
      },
    });
    await wrapper.get("button[title='removeMapping']").trigger("click");
    expect(wrapper.emitted("save-open-app")).toEqual([
      [{ defaultApp: "", mappings: [{ ext: "conf", app: "/apps/vim" }] }],
    ]);
  });

  it("shows a validation error for an invalid external app path", () => {
    const wrapper = mountPanel({ openAppError: "Choose an existing absolute executable path." });
    const appInput = wrapper.get("input[aria-label='externalApp']");
    expect(appInput.attributes("aria-invalid")).toBe("true");
    expect(wrapper.get("[role='alert']").text()).toContain("absolute executable");
  });

  it("hides the external app controls when local saving is unavailable", () => {
    const wrapper = mountPanel({ canSaveLocal: false, openApp: { defaultApp: "/apps/np", mappings: [] } });
    expect(wrapper.find("input[aria-label='externalApp']").exists()).toBe(false);
    expect(wrapper.text()).toContain("downloadDirectoryUnavailable");
  });
});
