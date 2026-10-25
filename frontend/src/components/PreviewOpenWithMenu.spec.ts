// @vitest-environment happy-dom
// 预览「打开方式」下拉：条目集合（系统默认/可用编辑器/自定义编辑器/自定义命令）、
// 选中即 emit、外点/Esc 关闭、本机能力缺失时禁用。
import { afterEach, describe, expect, it } from "vitest";
import { config, mount, type VueWrapper } from "@vue/test-utils";
import PreviewOpenWithMenu from "./PreviewOpenWithMenu.vue";
import type { EditorConfig, KnownEditor } from "../lib/editorRules";
import { vTip } from "../lib/tooltip";

config.global.directives = { tip: vTip };

const editors: KnownEditor[] = [
  {
    id: "vscode",
    name: "VS Code",
    available: true,
    launch: { kind: "exe", exe: "/usr/local/bin/code", args: [] },
    suggestedExtensions: ["rs"],
  },
  {
    id: "sublime",
    name: "Sublime Text",
    available: false,
    launch: { kind: "exe", exe: "/opt/sublime/sublime_text", args: [] },
    suggestedExtensions: [],
  },
];

const editorConfig: EditorConfig = {
  associations: [],
  customEditors: [{ id: "my-editor", name: "My Editor", command: "my-editor {file}" }],
  uploadPolicy: "auto",
};

let wrapper: VueWrapper | undefined;

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
});

async function flush() {
  for (let i = 0; i < 3; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

function mountMenu(overrides: Record<string, unknown> = {}) {
  wrapper = mount(PreviewOpenWithMenu, {
    props: {
      fileName: "main.rs",
      editors,
      editorConfig,
      canOpen: true,
      t: (key: string) => key,
      ...overrides,
    },
    attachTo: document.body,
  });
  return wrapper;
}

async function openMenu() {
  await wrapper!.get("[data-test=preview-open-with]").trigger("click");
  await flush();
}

describe("PreviewOpenWithMenu", () => {
  it("stays closed until the trigger is clicked", async () => {
    mountMenu();
    expect(wrapper!.find("[data-test=preview-open-with-menu]").exists()).toBe(false);
    await openMenu();
    expect(wrapper!.find("[data-test=preview-open-with-menu]").exists()).toBe(true);
  });

  it("lists system default, available editors and custom editors, hiding unavailable ones", async () => {
    mountMenu();
    await openMenu();
    const text = wrapper!.findAll(".wb-open-with-menu [role=menuitem]").map((item) => item.text());
    expect(text).toContain("sftpEdit.systemDefault");
    expect(text).toContain("VS Code");
    expect(text).toContain("My Editor");
    expect(text).toContain("sftpEdit.customCommand");
    expect(text).not.toContain("Sublime Text");
  });

  it("emits the picked known editor and closes", async () => {
    mountMenu();
    await openMenu();
    const vscode = wrapper!.findAll(".wb-open-with-menu [role=menuitem]").find((item) => item.text() === "VS Code");
    await vscode!.trigger("click");
    await flush();
    expect(wrapper!.emitted("open")).toEqual([[{ kind: "known", editor: editors[0] }]]);
    expect(wrapper!.find("[data-test=preview-open-with-menu]").exists()).toBe(false);
  });

  it("emits the custom command entry", async () => {
    mountMenu();
    await openMenu();
    const custom = wrapper!.findAll(".wb-open-with-menu [role=menuitem]").find((item) => item.text() === "sftpEdit.customCommand");
    await custom!.trigger("click");
    await flush();
    expect(wrapper!.emitted("custom-command")).toHaveLength(1);
  });

  it("closes on Escape without bubbling to the preview close chain", async () => {
    mountMenu();
    await openMenu();
    let reachedParent = false;
    const onKeydown = () => {
      reachedParent = true;
    };
    document.addEventListener("keydown", onKeydown);
    const escape = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true });
    document.dispatchEvent(escape);
    await flush();
    document.removeEventListener("keydown", onKeydown);
    expect(wrapper!.find("[data-test=preview-open-with-menu]").exists()).toBe(false);
    expect(reachedParent).toBe(false);
  });

  it("disables the trigger when local save capability is missing", () => {
    mountMenu({ canOpen: false, disabledHint: "desktopOnlyTitle" });
    expect(wrapper!.get("[data-test=preview-open-with]").attributes("disabled")).toBeDefined();
  });
});
