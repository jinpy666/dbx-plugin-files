// @vitest-environment happy-dom
import { beforeEach, describe, expect, it } from "vitest";
import { nextTick } from "vue";
import { mount } from "@vue/test-utils";
import FileTable from "./FileTable.vue";
import type { FileEntry } from "../lib/api";
import { loadUiPrefs, UI_PREFS_KEY } from "../lib/prefs";

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

describe("FileTable 媒体图标着色与空态（对标 rclone-dashboard）", () => {
  function mountMediaTable() {
    return mount(FileTable, {
      props: {
        entries: [
          { name: "photo.png", path: "/photo.png", kind: "file", size: 8 },
          { name: "clip.mp4", path: "/clip.mp4", kind: "file", size: 9 },
          { name: "song.mp3", path: "/song.mp3", kind: "file", size: 10 },
          { name: "main.go", path: "/main.go", kind: "file", size: 11 },
          { name: "backup.zip", path: "/backup.zip", kind: "file", size: 12 },
          { name: "notes.txt", path: "/notes.txt", kind: "file", size: 13 },
          { name: "docs", path: "/docs", kind: "directory", size: 0 },
        ],
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
  }

  function rowByName(wrapper: ReturnType<typeof mountMediaTable>, name: string) {
    return wrapper.findAll(".wb-file-row").find((row) => row.text().includes(name))!;
  }

  it("media/code/archive rows carry theme-aware icon classes; plain files stay neutral", () => {
    const wrapper = mountMediaTable();
    const colored = ["photo.png", "clip.mp4", "song.mp3", "main.go", "backup.zip"];
    for (const name of colored) {
      const classes = rowByName(wrapper, name).find("svg").attributes("class") ?? "";
      expect(classes.split(/\s+/).some((cls) => cls.startsWith("wb-fi-")), name).toBe(true);
    }
    expect(rowByName(wrapper, "notes.txt").find("svg").attributes("class") ?? "").not.toContain("wb-fi-");
    expect(rowByName(wrapper, "docs").find("svg").attributes("class") ?? "").toContain("wb-icon-dir");
    wrapper.unmount();
  });

  it("empty state renders a quiet card with an icon (FolderOpen for empty dir, SearchX for filter miss)", async () => {
    const wrapper = mountMediaTable();
    await wrapper.setProps({ entries: [] });
    expect(wrapper.find(".wb-file-empty svg").exists()).toBe(true);
    expect(wrapper.get(".wb-file-empty").text()).toBe("emptyDirectory");
    await wrapper.setProps({ filtered: true });
    expect(wrapper.get(".wb-file-empty").text()).toBe("noMatchResults");
    wrapper.unmount();
  });
});

// —— 列自定义（对标 WinSCP/Finder）：列宽拖拽 + 列显隐，持久化到 dbx-files.ui ——
function pointerEvent(type: string, clientX: number): Event {
  // happy-dom 无需真 PointerEvent：处理器只读 clientX/button/currentTarget。
  return new MouseEvent(type, { bubbles: true, cancelable: true, clientX });
}

function storedColumns(): Record<string, unknown> | undefined {
  const raw = window.localStorage.getItem(UI_PREFS_KEY);
  return raw ? (JSON.parse(raw).columns as Record<string, unknown>) : undefined;
}

function dragGrip(wrapper: ReturnType<typeof mountTable>, column: string, fromX: number, toX: number) {
  const grip = wrapper.get(`[data-test="resize-${column}"]`).element;
  grip.dispatchEvent(pointerEvent("pointerdown", fromX));
  grip.dispatchEvent(pointerEvent("pointermove", toX));
  grip.dispatchEvent(pointerEvent("pointerup", toX));
}

describe("FileTable 列宽拖拽（对标 WinSCP）", () => {
  it("拖拽调宽后写入 prefs，且表头与行单元格同步换宽", async () => {
    const wrapper = mountTable();
    dragGrip(wrapper, "name", 100, 160);
    const stored = storedColumns();
    expect(stored?.nameWidth).toBe(300); // 默认 240 + 60
    await nextTick(); // Vue 批量渲染：落盘后等待 DOM 应用新宽度
    expect(wrapper.get('[role="columnheader"]').attributes("style")).toContain("width: 300px");
    expect(wrapper.get(".wb-file-name").attributes("style")).toContain("width: 300px");
    wrapper.unmount();
  });

  it("低于列最小宽度时按最小值钳制（名称 120）", () => {
    const wrapper = mountTable();
    dragGrip(wrapper, "name", 100, -600);
    expect(storedColumns()?.nameWidth).toBe(120);
    wrapper.unmount();
  });

  it("松手才落盘：拖拽中不写 storage", () => {
    const wrapper = mountTable();
    const grip = wrapper.get('[data-test="resize-size"]').element;
    grip.dispatchEvent(pointerEvent("pointerdown", 100));
    grip.dispatchEvent(pointerEvent("pointermove", 150));
    expect(storedColumns()).toBeUndefined();
    grip.dispatchEvent(pointerEvent("pointerup", 150));
    expect(storedColumns()?.sizeWidth).toBe(140); // 默认 90 + 50
    wrapper.unmount();
  });

  it("落盘为读-改-写：保留存储内其他键（sort 等）", () => {
    window.localStorage.setItem(
      UI_PREFS_KEY,
      JSON.stringify({ sort: { column: "size", direction: "desc" }, auditActionFilter: "delete" }),
    );
    const wrapper = mountTable();
    dragGrip(wrapper, "size", 100, 150);
    const raw = JSON.parse(window.localStorage.getItem(UI_PREFS_KEY)!);
    expect(raw.sort).toEqual({ column: "size", direction: "desc" });
    expect(raw.auditActionFilter).toBe("delete");
    expect(raw.columns.sizeWidth).toBe(140);
    wrapper.unmount();
  });

  it("点击未拖动不落盘", () => {
    const wrapper = mountTable();
    const grip = wrapper.get('[data-test="resize-modified"]').element;
    grip.dispatchEvent(pointerEvent("pointerdown", 100));
    grip.dispatchEvent(pointerEvent("pointerup", 100));
    expect(storedColumns()).toBeUndefined();
    wrapper.unmount();
  });
});

describe("FileTable 列显隐（表头右键菜单）", () => {
  async function openMenu(wrapper: ReturnType<typeof mountTable>) {
    await wrapper.get(".wb-file-header").trigger("contextmenu", { clientX: 24, clientY: 12 });
    return wrapper.findAll('[data-test="column-menu-item"]');
  }

  it("表头右键弹出列菜单：名称列锁定常显，其余列可复选", async () => {
    const wrapper = mountTable();
    const items = await openMenu(wrapper);
    expect(items.length).toBe(3);
    expect(items[0].text()).toBe("colName");
    expect(items[0].attributes("aria-checked")).toBe("true");
    expect(items[0].attributes("disabled")).toBeDefined();
    expect(items[1].attributes("aria-checked")).toBe("true");
    expect(items[1].attributes("disabled")).toBeUndefined();
    wrapper.unmount();
  });

  it("表头右键不再触发 blank-context；列表空白区右键仍触发", async () => {
    const wrapper = mountTable();
    await wrapper.get(".wb-file-header").trigger("contextmenu", { clientX: 24, clientY: 12 });
    expect(wrapper.find('[data-test="column-menu"]').exists()).toBe(true);
    expect(wrapper.emitted("blank-context")).toBeUndefined();
    await wrapper.get(".wb-file-scroll").trigger("contextmenu", { clientX: 24, clientY: 60 });
    expect(wrapper.emitted("blank-context")).toEqual([[{ x: 24, y: 60 }]]);
    wrapper.unmount();
  });

  it("隐藏大小列：表头与行单元格同步移除并持久化", async () => {
    const wrapper = mountTable();
    const items = await openMenu(wrapper);
    await items[1].trigger("click"); // colSize
    expect(wrapper.findAll('[role="columnheader"]').length).toBe(2);
    expect(wrapper.findAll(".wb-file-row .wb-numeric").length).toBe(0);
    expect(storedColumns()?.hidden).toEqual(["size"]);
    wrapper.unmount();
  });

  it("刷新页面后保持（重新挂载读同一 storage）", async () => {
    const first = mountTable();
    const items = await openMenu(first);
    await items[2].trigger("click"); // colModified
    first.unmount();

    const second = mountTable();
    expect(second.findAll('[role="columnheader"]').length).toBe(2);
    expect(second.findAll(".wb-file-row .wb-muted").length).toBe(0);
    second.unmount();
  });

  it("再次点击恢复显示并清除持久化隐藏项", async () => {
    const wrapper = mountTable();
    let items = await openMenu(wrapper);
    await items[1].trigger("click");
    expect(wrapper.findAll('[role="columnheader"]').length).toBe(2);
    items = await openMenu(wrapper);
    expect(items[1].attributes("aria-checked")).toBe("false");
    await items[1].trigger("click");
    expect(wrapper.findAll('[role="columnheader"]').length).toBe(3);
    expect(wrapper.findAll(".wb-file-row .wb-numeric").length).toBe(3);
    expect(storedColumns()?.hidden).toEqual([]);
    wrapper.unmount();
  });

  it("名称列点击无效（不可隐藏）", async () => {
    const wrapper = mountTable();
    const items = await openMenu(wrapper);
    await items[0].trigger("click");
    expect(wrapper.findAll('[role="columnheader"]').length).toBe(3);
    expect(storedColumns() ?? {}).toEqual({});
    wrapper.unmount();
  });

  it("双栏实例共享全局列配置：一侧隐藏，另一侧即时同步", async () => {
    const left = mountTable();
    const right = mountTable();
    const items = await openMenu(left);
    await items[1].trigger("click");
    await nextTick();
    expect(right.findAll('[role="columnheader"]').length).toBe(2);
    left.unmount();
    right.unmount();
  });

  it("隐藏列后排序点击与 aria-sort 仅作用于可见列且不回归", async () => {
    const wrapper = mountTable();
    const items = await openMenu(wrapper);
    await items[1].trigger("click");
    const headers = wrapper.findAll('[role="columnheader"]');
    expect(headers.length).toBe(2);
    expect(headers[0].attributes("aria-sort")).toBe("ascending"); // 名称列仍是排序列
    await wrapper.get('button[type="button"]').trigger("click"); // 名称列排序按钮
    expect(wrapper.emitted("sort")).toEqual([["name"]]);
    wrapper.unmount();
  });
});

// issue #49：后端封顶截断时 footer 条目数以 N+ 提示并非完整清单。
describe("FileTable truncated footer (issue #49)", () => {
  it("footer shows plain count when not truncated", () => {
    const wrapper = mountTable();
    expect(wrapper.find(".wb-file-footer span").text()).toBe("entriesCount");
    wrapper.unmount();
  });

  it("footer appends + when truncated", () => {
    const wrapper = mount(FileTable, {
      props: {
        entries,
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        truncated: true,
        t: (key: string, values?: Record<string, string | number>) => (values ? `${key}:${values.count}` : key),
      },
    });
    expect(wrapper.find(".wb-file-footer span").text()).toBe("entriesCount:3+");
    wrapper.unmount();
  });
});
