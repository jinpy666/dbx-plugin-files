// @vitest-environment happy-dom
import { beforeEach, describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import FileTable from "./FileTable.vue";
import type { FileEntry } from "../lib/api";

const entries: FileEntry[] = [
  { name: "a.txt", path: "/a.txt", kind: "file", size: 1 },
  { name: "b.txt", path: "/b.txt", kind: "file", size: 2 },
  { name: "c.txt", path: "/c.txt", kind: "file", size: 3 },
];

function mountTable() {
  return mount(FileTable, {
    props: {
      entries,
      selection: [],
      activePath: "",
      sort: { column: "name", direction: "asc" },
      t: (key: string) => key,
    },
  });
}

function pressKeydown(wrapper: ReturnType<typeof mountTable>, key: string) {
  // 直接对滚动容器派发 keydown（与真实焦点位置一致）；不 await 渲染，
  // 复现「同 tick 内连续按键、props 尚未回写」的时序。
  const viewport = wrapper.find(".wb-file-scroll");
  viewport.element.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
}

beforeEach(() => {
  window.localStorage.clear();
});

describe("FileTable keyboard selection (P2-9 Space 首按回归)", () => {
  it("first Space right after ArrowDown toggles selection (stale activePath no longer dropped)", () => {
    const wrapper = mountTable();
    pressKeydown(wrapper, "ArrowDown");
    pressKeydown(wrapper, " ");
    // props.activePath 仍为 ""（同 tick），修复后用 nav.index 兜底 → 首按即勾选首行
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["/a.txt"]]);
  });

  it("second Space untoggles once props catch up (toggle semantics intact)", async () => {
    const wrapper = mountTable();
    pressKeydown(wrapper, "ArrowDown");
    pressKeydown(wrapper, " ");
    await wrapper.setProps({ activePath: "/a.txt", selection: ["/a.txt"] });
    pressKeydown(wrapper, " ");
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([[]]);
  });

  it("Space without any active row is a no-op that still prevents default scroll", () => {
    const wrapper = mountTable();
    const viewport = wrapper.find(".wb-file-scroll");
    const event = new KeyboardEvent("keydown", { key: " ", bubbles: true, cancelable: true });
    viewport.element.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(wrapper.emitted("update:selection")).toBeUndefined();
  });
});

describe("FileTable 空态与 a11y（R3-P2-6 / R3-P2-8）", () => {
  it("empty + filtered shows the no-match message instead of the empty-folder message (R3-P2-6)", () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: [],
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        filtered: true,
        t: (key: string) => key,
      },
    });
    expect(wrapper.find(".wb-file-empty").text()).toBe("noMatchResults");
  });

  it("empty without filter keeps the empty-folder message", () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: [],
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    expect(wrapper.find(".wb-file-empty").text()).toBe("emptyDirectory");
  });

  it("rows expose option semantics with aria-selected (R3-P2-8)", () => {
    const wrapper = mount(FileTable, {
      props: {
        entries,
        selection: ["/a.txt"],
        activePath: "/a.txt",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    const viewport = wrapper.find(".wb-file-scroll");
    expect(viewport.attributes("role")).toBe("listbox");
    expect(viewport.attributes("aria-label")).toBe("fileListLabel");
    const row = wrapper.find(".wb-file-row");
    expect(row.attributes("role")).toBe("option");
    expect(row.attributes("aria-selected")).toBe("true");
    const checkbox = row.find('input[type="checkbox"]');
    expect(checkbox.attributes("aria-label")).toBe("selectEntry");
  });

  it("column headers carry aria-sort for the active sort column (R3-P2-8)", async () => {
    const wrapper = mountTable();
    const headers = wrapper.findAll('[role="columnheader"]');
    expect(headers.length).toBe(3);
    expect(headers[0].attributes("aria-sort")).toBe("ascending");
    expect(headers[1].attributes("aria-sort")).toBeUndefined();
    await wrapper.setProps({ sort: { column: "size", direction: "desc" } });
    expect(wrapper.findAll('[role="columnheader"]')[1].attributes("aria-sort")).toBe("descending");
    expect(wrapper.findAll('[role="columnheader"]')[0].attributes("aria-sort")).toBeUndefined();
  });
});
