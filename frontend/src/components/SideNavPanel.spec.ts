// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mount, config } from "@vue/test-utils";
import SideNavPanel from "./SideNavPanel.vue";
import { createTreeRoot, type DirTreeNode } from "../lib/dirTree";
import type { QuickPath } from "../lib/quickPaths";
import { vTip } from "../lib/tooltip";

// 模板里的 v-tip（图标按钮提示）在测试挂载时同样需要指令注册。
config.global.directives = { tip: vTip };

function node(path: string, name: string, children: DirTreeNode[] = []): DirTreeNode {
  const created = createTreeRoot(path, name);
  created.children = children;
  created.loaded = true;
  created.expanded = children.length > 0;
  return created;
}

const treeRoot = node("/", "/", [
  node("/docs", "docs"),
  node("/empty", "empty"),
]);

const t = (key: string) => key;

function mountPanel() {
  // attachTo：happy-dom 的 focus() 只对已挂进文档的元素生效（roving focus 断言前提）。
  return mount(SideNavPanel, {
    attachTo: document.body,
    props: {
      side: "left",
      tab: "tree",
      collapsed: false,
      treeRoot,
      quickPaths: [] as QuickPath[],
      currentPath: "/docs",
      t,
    },
  });
}

function rows(wrapper: ReturnType<typeof mountPanel>) {
  return wrapper.findAll(".wb-tree-row");
}

function press(wrapper: ReturnType<typeof mountPanel>, key: string): KeyboardEvent {
  const body = wrapper.find(".wb-side-body");
  const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true });
  body.element.dispatchEvent(event);
  return event;
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  document.body.innerHTML = "";
});

describe("SideNavPanel tree keyboard access (P2-9)", () => {
  it("exposes the tree container as a focusable tree role with focusable rows", () => {
    const wrapper = mountPanel();
    expect(wrapper.find(".wb-side-body").attributes("tabindex")).toBe("0");
    expect(wrapper.find(".wb-side-body").attributes("role")).toBe("tree");
    for (const row of rows(wrapper)) {
      expect(row.attributes("tabindex")).toBe("-1");
    }
  });

  it("ArrowDown moves roving focus onto mounted rows in order", () => {
    const wrapper = mountPanel();
    press(wrapper, "ArrowDown");
    expect(document.activeElement).toBe(rows(wrapper)[0].element);
    press(wrapper, "ArrowDown");
    expect(document.activeElement).toBe(rows(wrapper)[1].element);
  });

  it("ArrowUp from the last row walks backwards", () => {
    const wrapper = mountPanel();
    press(wrapper, "ArrowUp");
    expect(document.activeElement).toBe(rows(wrapper).at(-1)!.element);
  });

  it("Enter on a focused row opens (navigates into) the directory", () => {
    const wrapper = mountPanel();
    press(wrapper, "ArrowDown");
    press(wrapper, "Enter");
    const navigations = wrapper.emitted("navigate");
    expect(navigations?.length).toBe(1);
    expect(navigations![0][0]).toBe("/");
  });

  it("ArrowRight expands the focused row via its caret", () => {
    const wrapper = mountPanel();
    const body = wrapper.find(".wb-side-body");
    const caret = rows(wrapper)[0].element.querySelector<HTMLButtonElement>(".wb-tree-caret");
    let toggles = 0;
    caret?.addEventListener("click", () => (toggles += 1), { once: true });
    (rows(wrapper)[0].element as HTMLElement).focus();
    body.element.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
    expect(toggles).toBe(1);
  });
});
