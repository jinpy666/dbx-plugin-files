// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";
import { mount } from "@vue/test-utils";
import FileTable from "./FileTable.vue";
import type { FileEntry } from "../lib/api";
import { loadUiPrefs, prefsStore, UI_PREFS_KEY } from "../lib/prefs";

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
  // 默认持久化后端是 prefsStore（宿主 storage 适配），播种/清理须走同一实例。
  prefsStore.removeItem(UI_PREFS_KEY);
});

describe("FileTable keyboard selection (P2-9 Space 首按回归)", () => {
  it("first Space right after ArrowDown toggles selection (stale activePath no longer dropped)", () => {
    const wrapper = mountTable();
    pressKeydown(wrapper, "ArrowDown");
    pressKeydown(wrapper, " ");
    // props.activePath 仍为 ""（同 tick），修复后用 nav.index 兜底 → 首按即勾选首行
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt"]]);
  });

  it("second Space untoggles once props catch up (toggle semantics intact)", async () => {
    const wrapper = mountTable();
    pressKeydown(wrapper, "ArrowDown");
    pressKeydown(wrapper, " ");
    await wrapper.setProps({ activePath: "/a.txt", selection: ["file:/a.txt"] });
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
  it("bucket entries render the bucket icon, plain dirs keep the folder icon", () => {
    // namespace 连接根层（engine/bucket_ns）：`bucket:true` 的目录条目是桶。
    const wrapper = mount(FileTable, {
      props: {
        entries: [
          { name: "photos", path: "/photos", kind: "directory", bucket: true },
          { name: "docs", path: "/docs", kind: "directory" },
          { name: "a.txt", path: "/a.txt", kind: "file" },
        ],
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    const html = wrapper.html();
    expect(html).toContain("lucide-database");
    expect(html).toContain("lucide-folder");
  });

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

  // issue #81：namespace 连接（bucketRoot）的根视图空态解释「为什么桶列
  // 表可能为空」（S3 兼容网关按 endpoint 区域返回桶）。子目录、过滤态与
  // 非 namespace 连接都不显示，避免把普通空目录误读成桶列表问题。
  it("bucket namespace root empty state explains the region-gated listing", () => {
    const mountBucket = (activePath: string) => mount(FileTable, {
      props: {
        entries: [],
        selection: [],
        activePath,
        sort: { column: "name", direction: "asc" },
        bucketRoot: true,
        t: (key: string) => key,
      },
    });
    const root = mountBucket("/");
    expect(root.find(".wb-empty-hint").text()).toBe("bucketListEmptyHint");
    root.unmount();
    const sub = mountBucket("/bucket");
    expect(sub.find(".wb-empty-hint").exists()).toBe(false);
    sub.unmount();
    const plain = mount(FileTable, {
      props: {
        entries: [],
        selection: [],
        activePath: "/",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    expect(plain.find(".wb-empty-hint").exists()).toBe(false);
    plain.unmount();
  });

  // 空目录引导（UX）：可写连接的空目录在空态卡片内给「新建文件夹」直达；
  // 过滤无匹配态与只读连接不显示。
  it("empty directory offers the new-folder CTA when writable, hidden when filtered or read-only", async () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: [],
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        canWrite: true,
        t: (key: string) => key,
      },
    });
    const cta = wrapper.find(".wb-empty-cta");
    expect(cta.exists()).toBe(true);
    expect(cta.text()).toContain("newFolder");
    await cta.trigger("click");
    expect(wrapper.emitted("new-folder")).toEqual([[]]);
    await wrapper.setProps({ filtered: true });
    expect(wrapper.find(".wb-empty-cta").exists()).toBe(false);
    await wrapper.setProps({ filtered: false, canWrite: false });
    expect(wrapper.find(".wb-empty-cta").exists()).toBe(false);
    wrapper.unmount();
  });

  it("rows expose option semantics with aria-selected (R3-P2-8)", () => {
    const wrapper = mount(FileTable, {
      props: {
        entries,
        selection: ["file:/a.txt"],
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

  // issue #82：对象存储同名文件+前缀目录共存（`new-api` 对象 + `new-api/` 前缀组，
  // 两条目 path 同值、kind 相异）——选中态以 kind+path 身份 key 隔离，互不覆盖。
  const clashEntries: FileEntry[] = [
    { name: "new-api", path: "/new-api", kind: "directory" },
    { name: "new-api", path: "/new-api", kind: "file", size: 93 },
  ];

  it("same-name file+dir rows toggle selection independently (checkbox)", async () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: clashEntries,
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    const rows = wrapper.findAll(".wb-file-row");
    expect(rows.length).toBe(2);
    await rows[0].find('input[type="checkbox"]').trigger("change");
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["dir:/new-api"]]);
    // props 回写后只勾目录行：文件行的 checkbox 与 aria-selected 都保持未选。
    await wrapper.setProps({ selection: ["dir:/new-api"] });
    const checked = wrapper.findAll(".wb-file-row").map((row) => (row.find('input[type="checkbox"]').element as HTMLInputElement).checked);
    expect(checked).toEqual([true, false]);
    expect(wrapper.findAll(".wb-file-row")[1].attributes("aria-selected")).toBe("false");
    // 再勾文件行：两实体同存于选择集，而不是覆盖目录行。
    await wrapper.findAll(".wb-file-row")[1].find('input[type="checkbox"]').trigger("change");
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["dir:/new-api", "file:/new-api"]]);
    wrapper.unmount();
  });

  it("right-click on one same-name entity selects only that entity (contextmenu)", async () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: clashEntries,
        selection: ["dir:/new-api"],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    // 右键文件行（未在选中集）：选择集整体切到该文件实体。
    await wrapper.findAll(".wb-file-row")[1].trigger("contextmenu");
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/new-api"]]);
    const menu = wrapper.emitted("contextmenu")?.at(-1)?.[0] as { entry: FileEntry };
    expect(menu.entry.kind).toBe("file");
    wrapper.unmount();
  });

  it("select-all carries both same-name entities as distinct keys", async () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: clashEntries,
        selection: [],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    pressKeydown(wrapper, "ArrowDown");
    const viewport = wrapper.find(".wb-file-scroll");
    viewport.element.dispatchEvent(new KeyboardEvent("keydown", { key: "a", metaKey: true, bubbles: true, cancelable: true }));
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["dir:/new-api", "file:/new-api"]]);
    wrapper.unmount();
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
    await wrapper.setProps({ canWrite: true, selection: ["file:/b.txt"], activePath: "/b.txt" });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'Delete' });
    expect(wrapper.emitted('delete')).toEqual([[]]);
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'F2' });
    expect(wrapper.emitted('rename')).toEqual([[entries[1]]]);
    await wrapper.setProps({ selection: ["file:/a.txt", "file:/b.txt"] });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'F2' });
    expect(wrapper.emitted('rename')).toHaveLength(1);
    wrapper.unmount();
  });

  it.each([
    { canWrite: false }, { loading: true }, { failed: true }, { selection: [] },
  ])("does not request write actions when unavailable: %j", async (props) => {
    const wrapper = mountTable();
    await wrapper.setProps({ canWrite: true, selection: ["file:/a.txt"], ...props });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'Delete' });
    await wrapper.get('.wb-file-scroll').trigger('keydown', { key: 'F2' });
    expect(wrapper.emitted('delete')).toBeUndefined();
    expect(wrapper.emitted('rename')).toBeUndefined();
    wrapper.unmount();
  });

  it.each(['metaKey', 'ctrlKey', 'altKey', 'shiftKey', 'repeat', 'isComposing'])("leaves %s shortcuts to their owner", async (modifier) => {
    const wrapper = mountTable();
    await wrapper.setProps({ canWrite: true, selection: ["file:/a.txt"] });
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
    await wrapper.setProps({ canWrite: true, selection: ["file:/a.txt"], activePath: "/a.txt" });
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
    await wrapper.setProps({ entries: cached, failed: true, selection: ['file:/a.txt'] });
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

// —— 虚拟窗口：视口高度是响应式状态，ResizeObserver 报告变化后窗口切片即重算 ——
describe("FileTable 虚拟窗口（视口高度响应式）", () => {
  it("视口拉大后窗口切片随之扩大，底部不再留白到下一次 scroll", async () => {
    let observerCallback: ResizeObserverCallback = () => {};
    class RecordingResizeObserver {
      constructor(cb: ResizeObserverCallback) {
        observerCallback = cb;
      }
      observe() {}
      unobserve() {}
      disconnect() {}
    }
    vi.stubGlobal("ResizeObserver", RecordingResizeObserver);
    const many: FileEntry[] = Array.from({ length: 120 }, (_, i) => ({
      name: `f${i}.txt`,
      path: `/f${i}.txt`,
      kind: "file",
      size: i,
    }));
    const wrapper = mount(FileTable, {
      props: { entries: many, selection: [], activePath: "", sort: { column: "name", direction: "asc" }, t: (key: string) => key },
    });
    const scrollEl = wrapper.get(".wb-file-scroll").element as HTMLElement;
    // happy-dom 无真实布局：clientHeight 恒为 0，用 defineProperty 模拟高度变化。
    const setHeight = (height: number) =>
      Object.defineProperty(scrollEl, "clientHeight", { configurable: true, value: height });
    setHeight(280);
    observerCallback([], {} as ResizeObserver);
    await nextTick();
    expect(wrapper.findAll(".wb-file-row").length).toBe(Math.ceil(280 / 28) + 16);
    setHeight(1120);
    observerCallback([], {} as ResizeObserver);
    await nextTick();
    expect(wrapper.findAll(".wb-file-row").length).toBe(Math.ceil(1120 / 28) + 16);
    wrapper.unmount();
    vi.unstubAllGlobals();
  });
});
// —— 列自定义（对标 WinSCP/Finder）：列宽拖拽 + 列显隐，持久化到 dbx-files.ui ——
function pointerEvent(type: string, clientX: number): Event {
  // happy-dom 无需真 PointerEvent：处理器只读 clientX/button/currentTarget。
  return new MouseEvent(type, { bubbles: true, cancelable: true, clientX });
}

function storedColumns(): Record<string, unknown> | undefined {
  const raw = prefsStore.getItem(UI_PREFS_KEY);
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
    prefsStore.setItem(
      UI_PREFS_KEY,
      JSON.stringify({ sort: { column: "size", direction: "desc" }, auditActionFilter: "delete" }),
    );
    const wrapper = mountTable();
    dragGrip(wrapper, "size", 100, 150);
    const raw = JSON.parse(prefsStore.getItem(UI_PREFS_KEY)!);
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

describe("FileTable 表头全选（#91）", () => {
  it("checking the header checkbox selects every loaded entry; unchecking clears", async () => {
    const wrapper = mountTable();
    const checkbox = wrapper.find('[data-test="select-all"]');
    expect(checkbox.exists()).toBe(true);
    await checkbox.setValue(true);
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt", "file:/b.txt", "file:/c.txt"]]);
    await checkbox.setValue(false);
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([[]]);
  });

  it("reflects indeterminate while a strict subset is selected", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ selection: ["file:/a.txt"] });
    await nextTick();
    const input = wrapper.find('[data-test="select-all"]').element as HTMLInputElement;
    expect(input.checked).toBe(false);
    expect(input.indeterminate).toBe(true);
    await wrapper.setProps({ selection: ["file:/a.txt", "file:/b.txt", "file:/c.txt"] });
    await nextTick();
    expect((wrapper.find('[data-test="select-all"]').element as HTMLInputElement).checked).toBe(true);
    expect((wrapper.find('[data-test="select-all"]').element as HTMLInputElement).indeterminate).toBe(false);
  });
});

// —— issue #91：行点击多选——Ctrl 是 Windows 上 Cmd+click 的唯一等价增减修饰键，
// 行点击必须转发 event.ctrlKey；Shift 区段与全选/Cmd+A 一致 emit kind+path 复合
// key（issue #82），裸 path 与 selectedSet 匹配不上会让区段选择整体失真。
describe("FileTable 行点击多选（#91）", () => {
  it("ctrl+click toggles the row into the selection instead of resetting to single", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ selection: ["file:/a.txt"] });
    await wrapper.findAll(".wb-file-row")[1].trigger("click", { ctrlKey: true });
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt", "file:/b.txt"]]);
    // 再 Ctrl+点同一行：增减语义，取消勾选。
    await wrapper.setProps({ selection: ["file:/a.txt", "file:/b.txt"] });
    await wrapper.findAll(".wb-file-row")[1].trigger("click", { ctrlKey: true });
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt"]]);
    wrapper.unmount();
  });

  it("shift+click selects the anchor..target range as kind+path keys (issue #82)", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ selection: ["file:/a.txt"] });
    await wrapper.findAll(".wb-file-row")[2].trigger("click", { shiftKey: true });
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt", "file:/b.txt", "file:/c.txt"]]);
    wrapper.unmount();
  });

  it("shift+click across same-name file+dir carries both entities as distinct keys", async () => {
    const wrapper = mount(FileTable, {
      props: {
        entries: [
          { name: "new-api", path: "/new-api", kind: "directory" },
          { name: "new-api", path: "/new-api", kind: "file", size: 93 },
        ],
        selection: ["dir:/new-api"],
        activePath: "",
        sort: { column: "name", direction: "asc" },
        t: (key: string) => key,
      },
    });
    await wrapper.findAll(".wb-file-row")[1].trigger("click", { shiftKey: true });
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["dir:/new-api", "file:/new-api"]]);
    wrapper.unmount();
  });
});

// —— issue #91 候选增强：空白区鼠标框选（marquee）——对标 FileZilla/Finder：
// 空白处按下拖动画矩形、按行索引划入选区；Ctrl/Cmd 并入既有选中；位移小于
// 阈值按「空白处单击」=清空；行上按下绝不触发框选（行有自己的点击/拖拽语义）。
// 测试环境 getBoundingClientRect 全 0、scrollTop 0，clientY 即内容坐标（行高 28）。
describe("FileTable 空白区框选（#91 候选增强）", () => {
  function pointerEvent(type: string, x: number, y: number, extra: MouseEventInit = {}) {
    return new MouseEvent(type, { bubbles: true, cancelable: true, button: 0, clientX: x, clientY: y, ...extra });
  }

  async function dragMarquee(
    wrapper: ReturnType<typeof mountTable>,
    from: [number, number],
    to: [number, number],
    extra: MouseEventInit = {},
  ) {
    const viewport = wrapper.find(".wb-file-scroll");
    viewport.element.dispatchEvent(pointerEvent("pointerdown", from[0], from[1], extra));
    await nextTick();
    viewport.element.dispatchEvent(pointerEvent("pointermove", to[0], to[1], extra));
    await nextTick();
    viewport.element.dispatchEvent(pointerEvent("pointerup", to[0], to[1], extra));
    await nextTick();
  }

  it("plain drag across rows selects the covered range as kind+path keys", async () => {
    const wrapper = mountTable();
    await dragMarquee(wrapper, [10, 5], [60, 70]); // row0 → row2
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt", "file:/b.txt", "file:/c.txt"]]);
    wrapper.unmount();
  });

  it("partial drag selects only the covered rows", async () => {
    const wrapper = mountTable();
    await dragMarquee(wrapper, [10, 30], [60, 60]); // row1 → row2
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/b.txt", "file:/c.txt"]]);
    wrapper.unmount();
  });

  it("ctrl-drag unions the marquee range into the existing selection", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ selection: ["file:/a.txt"] });
    await dragMarquee(wrapper, [10, 30], [60, 60], { ctrlKey: true }); // row1 → row2
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([["file:/a.txt", "file:/b.txt", "file:/c.txt"]]);
    wrapper.unmount();
  });

  it("a sub-threshold blank press behaves as a click and clears the selection", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ selection: ["file:/a.txt"] });
    await dragMarquee(wrapper, [10, 90], [12, 91]); // 位移 2px < 阈值
    expect(wrapper.emitted("update:selection")?.at(-1)).toEqual([[]]);
    wrapper.unmount();
  });

  it("a drag that starts on a row never opens a marquee", async () => {
    const wrapper = mountTable();
    await wrapper.setProps({ selection: ["file:/a.txt"] });
    const row = wrapper.findAll(".wb-file-row")[1];
    row.element.dispatchEvent(pointerEvent("pointerdown", 10, 35));
    await nextTick();
    wrapper.find(".wb-file-scroll").element.dispatchEvent(pointerEvent("pointermove", 60, 70));
    await nextTick();
    wrapper.find(".wb-file-scroll").element.dispatchEvent(pointerEvent("pointerup", 60, 70));
    await nextTick();
    expect(wrapper.emitted("update:selection")).toBeUndefined();
    wrapper.unmount();
  });
});

