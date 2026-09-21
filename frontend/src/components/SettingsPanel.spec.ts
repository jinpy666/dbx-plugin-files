// @vitest-environment happy-dom
// SettingsPanel 单测（统一保存模型）：草稿编辑不直接发事件，持久化统一走
// 暴露的 save()；transfer 的数字+单位组合、downloads/openWith 的草稿归一化、
// Web 端的「仅桌面」卡片与降级行为。
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

type Overrides = Partial<{
  canSaveLocal: boolean;
  saveDir: string;
  defaultSaveDir: string;
  downloadDirError: string;
  openApp: OpenAppPrefs;
  openAppError: string;
  bwlimit: string;
  bwlimitError: string;
}>;

function mountPanel(section: "downloads" | "openWith" | "transfer", overrides: Overrides = {}) {
  ensureHost();
  return mount(SettingsPanel, {
    props: {
      t,
      canSaveLocal: true,
      saveDir: "",
      defaultSaveDir: "/Users/me/Downloads",
      openApp: { defaultApp: "", mappings: [] },
      section,
      ...overrides,
    },
  });
}

async function saveViaExpose(wrapper: ReturnType<typeof mountPanel>) {
  await (wrapper.vm as unknown as { save: () => Promise<void> }).save();
}

afterEach(() => {
  vi.restoreAllMocks();
  if (window.dbxPlugin) Reflect.deleteProperty(window.dbxPlugin, "fileTransfer");
});

describe("SettingsPanel downloads section (collective save)", () => {
  it("keeps edits as draft and only emits save-dir through save()", async () => {
    const wrapper = mountPanel("downloads", { saveDir: "", defaultSaveDir: "/Users/me/Downloads" });
    const input = wrapper.get(".wb-settings-path-row input");
    expect(input.attributes("placeholder")).toBe("/Users/me/Downloads");
    await input.setValue("/tmp/drop");
    expect(wrapper.emitted("save-dir")).toBeUndefined();
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-dir")).toEqual([["/tmp/drop"]]);
  });

  it("emits an empty value when restored to default and then saved", async () => {
    const wrapper = mountPanel("downloads", { saveDir: "/tmp/drop" });
    await wrapper.get("button[title='restoreDefaultDirectory']").trigger("click");
    expect(wrapper.emitted("save-dir")).toBeUndefined();
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-dir")).toEqual([[""]]);
  });

  it("fills the draft from the host directory picker; path persists on save()", async () => {
    (window.dbxPlugin!.fileTransfer as unknown) = {
      ...(window.dbxPlugin!.fileTransfer ?? {}),
      pickDirectory: vi.fn().mockResolvedValue({ path: "/Volumes/Archive" }),
    };
    const wrapper = mountPanel("downloads", { saveDir: "" });
    await wrapper.get("button[title='chooseDirectory']").trigger("click");
    expect((wrapper.get(".wb-settings-path-row input").element as HTMLInputElement).value).toBe("/Volumes/Archive");
    expect(wrapper.emitted("save-dir")).toBeUndefined();
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-dir")).toEqual([["/Volumes/Archive"]]);
  });

  it("shows the desktop-only card on web instead of the controls", () => {
    const wrapper = mountPanel("downloads", { canSaveLocal: false });
    expect(wrapper.text()).toContain("desktopOnlyTitle");
    expect(wrapper.text()).toContain("desktopOnlyDesc");
    expect(wrapper.find(".wb-settings-path-row").exists()).toBe(false);
  });
});

describe("SettingsPanel openWith section (collective save)", () => {
  const base = { defaultApp: "", mappings: [] } as OpenAppPrefs;

  it("emits typed default app only via save(), with App-side sanitization input", async () => {
    const wrapper = mountPanel("openWith", { openApp: base });
    const appInput = wrapper.findAll(".wb-settings-path-row input")[0]!;
    await appInput.setValue("  /Apps/Vision.app  ");
    expect(wrapper.emitted("save-open-app")).toBeUndefined();
    await saveViaExpose(wrapper);
    const payload = wrapper.emitted("save-open-app")?.[0]?.[0] as OpenAppPrefs;
    expect(payload.defaultApp).toBe("/Apps/Vision.app");
  });

  it("normalizes mappings (trim/lowercase/strip dots) and drops half-filled rows on save", async () => {
    const wrapper = mountPanel("openWith", { openApp: base });
    const rows = wrapper.findAll(".wb-settings-path-row");
    // 现有默认应用行 + 新增一行映射。
    await wrapper.findAll(".wb-link-button")[0]!.trigger("click");
    const mappingRows = () => wrapper.findAll(".wb-settings-path-row");
    await mappingRows()[1]!.find("input").setValue(".PDF");
    await mappingRows()[1]!.findAll("input")[1]!.setValue(" /Apps/Preview.app ");
    // 追加一行半成品（只填扩展名），保存时应被过滤。
    await wrapper.findAll(".wb-link-button")[0]!.trigger("click");
    await mappingRows()[2]!.find("input").setValue(".md");
    expect(wrapper.emitted("save-open-app")).toBeUndefined();
    await saveViaExpose(wrapper);
    const payload = wrapper.emitted("save-open-app")?.[0]?.[0] as OpenAppPrefs;
    expect(payload.mappings).toEqual([
      { ext: "pdf", app: "/Apps/Preview.app" },
    ]);
  });

  it("restores the system default app by draft and emits empty on save", async () => {
    const wrapper = mountPanel("openWith", { openApp: { defaultApp: "/Apps/Vision.app", mappings: [] } });
    await wrapper.get("button[title='useSystemDefaultApp']").trigger("click");
    expect(wrapper.emitted("save-open-app")).toBeUndefined();
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-open-app")?.[0]?.[0]).toEqual({ defaultApp: "", mappings: [] });
  });

  it("shows the desktop-only card on web and hides the controls", () => {
    const wrapper = mountPanel("openWith", { canSaveLocal: false, openApp: base });
    expect(wrapper.text()).toContain("desktopOnlyTitle");
    expect(wrapper.find(".wb-settings-path-row").exists()).toBe(false);
  });
});

describe("SettingsPanel transfer section (number + unit combo)", () => {
  it("parses the persisted rate into number+unit and composes on save", async () => {
    const wrapper = mountPanel("transfer", { bwlimit: "10M" });
    expect((wrapper.get(".wb-bwlimit-combo input").element as HTMLInputElement).value).toBe("10");
    expect((wrapper.get(".wb-bwlimit-unit").element as HTMLSelectElement).value).toBe("M");
    await wrapper.get(".wb-bwlimit-combo input").setValue("5");
    await wrapper.get(".wb-bwlimit-unit").setValue("G");
    // 统一保存模型：面板内不再有单区保存按钮。
    expect(wrapper.find(".wb-bwlimit-combo button").exists()).toBe(false);
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-bwlimit")).toEqual([["5G"]]);
  });

  it("treats an empty number as unlimited on save", async () => {
    const wrapper = mountPanel("transfer", { bwlimit: "10M" });
    await wrapper.get(".wb-bwlimit-combo input").setValue("");
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-bwlimit")).toEqual([[""]]);
  });

  it("warns when the persisted rate is a split (up:down) value", async () => {
    const wrapper = mountPanel("transfer", { bwlimit: "1M:100k" });
    expect((wrapper.get(".wb-bwlimit-combo input").element as HTMLInputElement).value).toBe("1");
    expect(wrapper.get(".wb-settings-hint").text()).toContain("bwlimitSplitHint");
    await saveViaExpose(wrapper);
    expect(wrapper.emitted("save-bwlimit")?.[0]?.[0]).toBe("1M");
  });

  it("surfaces the sidecar rejection inline and watches pref updates", async () => {
    const wrapper = mountPanel("transfer", { bwlimit: "", bwlimitError: "rejected" });
    expect(wrapper.get('[role="alert"]').text()).toBe("rejected");
    await wrapper.setProps({ bwlimit: "1M:100k", bwlimitError: "" });
    expect(wrapper.find('[role="alert"]').exists()).toBe(false);
  });
});
