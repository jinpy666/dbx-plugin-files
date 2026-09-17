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

describe("FileTable 类型图标", () => {
  const typedEntries: FileEntry[] = [
    { name: "photo.png", path: "/photo.png", kind: "file", size: 1 },
    { name: "报告.docx", path: "/报告.docx", kind: "file", size: 2 },
    { name: "预算.xlsx", path: "/预算.xlsx", kind: "file", size: 3 },
    { name: "notes.txt", path: "/notes.txt", kind: "file", size: 4 },
    { name: "docs", path: "/docs", kind: "directory" },
  ];

  function mountTypedTable() {
    return mount(FileTable, {
      props: {
        entries: typedEntries,
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
  }

  it.each([
    [0, ".lucide-file-image"],
    [1, ".lucide-file-text"],
    [2, ".lucide-file-spreadsheet"],
    [3, ".lucide-file"],
  ])("行 %i 按扩展名渲染类型图标（%s）", (index, selector) => {
    const wrapper = mountTypedTable();
    const cells = wrapper.findAll(".wb-file-name");
    expect(cells[index].find(selector).exists()).toBe(true);
    wrapper.unmount();
  });

  it("目录仍渲染文件夹图标", () => {
    const wrapper = mountTypedTable();
    const cells = wrapper.findAll(".wb-file-name");
    expect(cells[4].find("svg.wb-icon-dir.lucide-folder").exists()).toBe(true);
    expect(cells[4].find(".lucide-file").exists()).toBe(false);
    wrapper.unmount();
  });

  it("类型图标对辅助技术隐藏", () => {
    const wrapper = mountTypedTable();
    const icon = wrapper.findAll(".wb-file-name")[0].find("svg");
    expect(icon.attributes("aria-hidden")).toBe("true");
    wrapper.unmount();
  });
});

describe("FileTable write shortcuts", () => {
  it("Delete requests confirmation and F2 targets exactly one selected entry", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ canWrite: true, selection: ["/b.txt"], activePath: "/b.txt" });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'Delete' });
    expect(wrapper.emitted('delete')).toEqual([[]]);
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'F2' });
    expect(wrapper.emitted('rename')).toEqual([[entries[1]]]);
    await wrapper.setProps({ selection: ["/a.txt", "/b.txt"] });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'F2' });
    expect(wrapper.emitted('rename')).toHaveLength(1);
    wrapper.unmount();
  });

  it.each([
    { canWrite: false }, { loading: true }, { failed: true }, { selection: [] },
  ])("does not request write actions when unavailable: %j", async (props) => {
    const wrapper = mountTable();
    await wrapper.setProps({ canWrite: true, selection: ["/a.txt"], ...props });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'Delete' });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'F2' });
    expect(wrapper.emitted('delete')).toBeUndefined();
    expect(wrapper.emitted('rename')).toBeUndefined();
    wrapper.unmount();
  });

  it.each(['metaKey', 'ctrlKey', 'altKey', 'shiftKey', 'repeat', 'isComposing'])("leaves %s shortcuts to their owner", async (modifier) => {
    const wrapper = mountTable();
    await wrapper.setProps({ canWrite: true, selection: ["/a.txt"] });
    for (const key of ['Delete', 'F2']) {
      const event = new KeyboardEvent('keydown', { key, [modifier]: true, bubbles: true, cancelable: true });
      wrapper.get('.wb-file-scroll').element.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(false);
    }
    expect(wrapper.emitted('delete')).toBeUndefined();
    expect(wrapper.emitted('rename')).toBeUndefined();
    wrapper.unmount();
  });

  it("does not steal Space or write shortcuts from a focused checkbox", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ canWrite: true, selection: ["/a.txt"], activePath: "/a.txt" });
    for (const key of [' ', 'Delete', 'F2']) {
      const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true });
      wrapper.get('input[type=checkbox]').element.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(false);
    }
    expect(wrapper.emitted('update:selection')).toBeUndefined();
    expect(wrapper.emitted('delete')).toBeUndefined();
    expect(wrapper.emitted('rename')).toBeUndefined();
    wrapper.unmount();
  });
});

describe("FileTable loading and failure states", () => {
  it("returns to visible loading feedback after scrolling and blocks hidden-row navigation", async () => {
    const wrapper = mountTable();
    const scroll = wrapper.get('.wb-file-scroll');
    (scroll.element as HTMLElement).scrollTop = 10_000;
    await scroll.trigger('scroll');
    await wrapper.setProps({ loading: true, activePath: '/a.txt' });
    expect((scroll.element as HTMLElement).scrollTop).toBe(0);
    expect(scroll.attributes('aria-busy')).toBe('true');
    expect(wrapper.get('.wb-file-footer [role=status]').text()).toBe('loading');
    expect(wrapper.text()).not.toContain('emptyDirectory');
    expect(wrapper.text()).not.toContain('entriesCount');
    await scroll.trigger('keydown', { key: 'Enter' });
    expect(wrapper.emitted('open')).toBeUndefined();
    wrapper.unmount();
  });

  it.each([{ cached: [] as FileEntry[] }, { cached: entries }])("distinguishes failure from an empty or cached listing: %j", async ({ cached }) => {
    const wrapper = mountTable();
    await wrapper.setProps({ entries: cached, failed: true, selection: ['/a.txt'] });
    expect(wrapper.get('[role=status]').text()).toContain('directoryLoadFailed');
    expect(wrapper.findAll('.wb-file-row')).toHaveLength(0);
    expect(wrapper.text()).not.toContain('emptyDirectory');
    expect(wrapper.text()).not.toContain('entriesCount');
    expect(wrapper.text()).not.toContain('selectedCount');
    await wrapper.get('[role=status] button').trigger('click');
    expect(wrapper.emitted('retry')).toEqual([[]]);
    await wrapper.setProps({ failed: false, entries: [] });
    expect(wrapper.get('.wb-file-empty').text()).toBe('emptyDirectory');
    wrapper.unmount();
  });
});
