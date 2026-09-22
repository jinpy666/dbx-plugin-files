<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { Check, Folder, FolderOpen, SearchX } from "@lucide/vue";
import { formatBytes, formatTime, type FileEntry } from "../lib/api";
import { fileIcon, fileIconClass } from "../lib/fileIcons";
import { listNav, scrollRowIntoView, selectionRange, type ListNavState } from "../lib/listNav";
import {
  clampColumnWidth,
  COLUMN_MIN_WIDTH,
  DEFAULT_COLUMN_PREFS,
  loadUiPrefs,
  onColumnPrefsChange,
  updateColumnPrefs,
  type ColumnPrefs,
} from "../lib/prefs";

const props = defineProps<{
  entries: FileEntry[];
  selection: string[];
  activePath: string;
  sort: { column: "name" | "size" | "modified"; direction: "asc" | "desc" };
  loading?: boolean;
  failed?: boolean;
  canWrite?: boolean;
  /** 双栏拖拽标识：写入 dataTransfer，接收端据此判断来源与目标。 */
  paneId?: string;
  /** R3-P2-6：当前处于文件名过滤态——空列表时区分「无匹配」与「空目录」。 */
  filtered?: boolean;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "update:selection", value: string[]): void;
  (event: "update:activePath", value: string): void;
  (event: "open", entry: FileEntry): void;
  (event: "delete"): void;
  (event: "rename", entry: FileEntry): void;
  (event: "retry"): void;
  (event: "contextmenu", payload: { entry: FileEntry; x: number; y: number }): void;
  /** 空白区右键（表头/列表空余处）：弹插件菜单（新建/刷新），拦截浏览器默认菜单。 */
  (event: "blank-context", payload: { x: number; y: number }): void;
  (event: "sort", column: "name" | "size" | "modified"): void;
  /** A-FILES ①：行拖拽开始，携带拖拽集（已选中项或单行）。 */
  (event: "drag-entries", payload: { paneId: string; entries: FileEntry[] }): void;
  /** parity-tools：多选时的批量重命名入口（底部按钮，键盘可达）。 */
  (event: "batch-rename"): void;
}>();

// 虚拟滚动：固定行高 + 视口窗口切片，万级条目列表也只渲染可见行。
const ROW_HEIGHT = 28;
const OVERSCAN = 8;
const SKELETON_ROWS = 12;
const scrollTop = ref(0);
const viewport = ref<HTMLElement>();

function onScroll() {
  if (viewport.value) scrollTop.value = viewport.value.scrollTop;
}

const totalHeight = computed(() => Math.max(props.entries.length, props.loading ? SKELETON_ROWS : 0) * ROW_HEIGHT);
const firstIndex = computed(() => Math.max(0, Math.floor(scrollTop.value / ROW_HEIGHT) - OVERSCAN));
const visibleCount = computed(() => Math.ceil((viewport.value?.clientHeight ?? 480) / ROW_HEIGHT) + OVERSCAN * 2);
const visibleEntries = computed(() => props.entries.slice(firstIndex.value, firstIndex.value + visibleCount.value));

watch(
  () => props.entries,
  () => {
    scrollTop.value = 0;
    nav.value = { index: -1, anchor: -1 };
    if (viewport.value) viewport.value.scrollTop = 0;
  },
);

// 刷新从顶部显示骨架；停留在大目录底部时不留下看不到反馈的空白区。
watch(() => props.loading, (loading) => {
  if (!loading) return;
  scrollTop.value = 0;
  if (viewport.value) viewport.value.scrollTop = 0;
});

// 键盘导航（对标 tiny-rdm/FileZilla）：↑↓/Home/End 移动，Shift 连续扩选，
// Enter 打开、Space 切换勾选、Cmd/Ctrl+A 全选；焦点在列表上即生效。
const nav = ref<ListNavState>({ index: -1, anchor: -1 });

function activeIndex() {
  return props.entries.findIndex((entry) => entry.path === props.activePath);
}

function onListKeydown(event: KeyboardEvent) {
  // 仅列表自身取焦时接管，保留复选框/输入控件的原生键盘行为。
  if (event.defaultPrevented || event.isComposing || event.target !== event.currentTarget || props.loading || props.failed || !props.entries.length) return;
  if (!event.metaKey && !event.ctrlKey && !event.altKey && !event.shiftKey && (event.key === "Delete" || event.key === "F2")) {
    if (!props.canWrite || event.repeat || !props.selection.length) return;
    if (event.key === "Delete") {
      event.preventDefault();
      event.stopPropagation();
      emit("delete");
    } else if (props.selection.length === 1) {
      const entry = props.entries.find((candidate) => candidate.path === props.selection[0]);
      if (!entry) return;
      event.preventDefault();
      event.stopPropagation();
      emit("rename", entry);
    }
    return;
  }
  if ((event.metaKey || event.ctrlKey) && (event.key === "a" || event.key === "A")) {
    event.preventDefault();
    nav.value = { index: props.entries.length - 1, anchor: 0 };
    emit("update:selection", props.entries.map((entry) => entry.path));
    return;
  }
  const navKeys: Record<string, "up" | "down" | "home" | "end"> = { ArrowUp: "up", ArrowDown: "down", Home: "home", End: "end" };
  const direction = navKeys[event.key];
  if (direction) {
    event.preventDefault();
    const next = listNav({ index: activeIndex(), anchor: nav.value.anchor }, direction, props.entries.length, event.shiftKey);
    nav.value = next;
    const [start, end] = selectionRange(next);
    emit("update:selection", props.entries.slice(start, end + 1).map((entry) => entry.path));
    emit("update:activePath", props.entries[next.index].path);
    if (viewport.value) viewport.value.scrollTop = scrollRowIntoView(viewport.value, next.index, ROW_HEIGHT);
    return;
  }
  if (event.key === "Enter" || event.key === " ") {
    // Space/Enter 首按偶发无效的根因（第 2 轮扫描定位）：activePath 由父组件
    // 经 props 回写，方向键按下后的同一渲染 tick 内 props 仍是旧值 →
    // activeIndex() 为 -1，首按 Space 被丢弃（第二次才“勾选”）。改用本组件
    // 同步维护的 nav.index 作权威判定；preventDefault 提前到 index 判定前，
    // 避免 Space 在列表滚动容器上触发默认滚动。
    event.preventDefault();
    let index = activeIndex();
    if (index < 0) index = nav.value.index;
    if (index < 0 || index >= props.entries.length) return;
    const entry = props.entries[index];
    if (event.key === "Enter") emit("open", entry);
    else toggleSelection(entry, { meta: true });
  }
}

const selectedSet = computed(() => new Set(props.selection));

function rowStyle(index: number) {
  return { top: `${index * ROW_HEIGHT}px` };
}

function toggleSelection(entry: FileEntry, modifiers: { meta?: boolean; ctrl?: boolean; shift?: boolean }) {
  const current = [...props.selection];
  if (modifiers.meta || modifiers.ctrl) {
    const index = current.indexOf(entry.path);
    if (index >= 0) current.splice(index, 1);
    else current.push(entry.path);
    emit("update:selection", current);
    return;
  }
  if (modifiers.shift && current.length && props.entries.length) {
    const anchorPath = current[current.length - 1];
    const anchorIndex = props.entries.findIndex((candidate) => candidate.path === anchorPath);
    const targetIndex = props.entries.indexOf(entry);
    if (anchorIndex >= 0 && targetIndex >= 0) {
      const [start, end] = anchorIndex < targetIndex ? [anchorIndex, targetIndex] : [targetIndex, anchorIndex];
      emit("update:selection", props.entries.slice(start, end + 1).map((candidate) => candidate.path));
      return;
    }
  }
  emit("update:selection", [entry.path]);
  emit("update:activePath", entry.path);
  // 鼠标单击重置键盘扩选锚点，保证随后 Shift+方向键从该行起算。
  const index = props.entries.indexOf(entry);
  nav.value = { index, anchor: index };
}

function onClickRow(entry: FileEntry, event: MouseEvent) {
  toggleSelection(entry, { meta: event.metaKey, shift: event.shiftKey });
}

function onContextRow(entry: FileEntry, event: MouseEvent) {
  if (!selectedSet.value.has(entry.path)) {
    emit("update:selection", [entry.path]);
  }
  emit("update:activePath", entry.path);
  emit("contextmenu", { entry, x: event.clientX, y: event.clientY });
}

function onDragStart(entry: FileEntry, event: DragEvent) {
  const dragged = selectedSet.value.has(entry.path)
    ? props.entries.filter((candidate) => selectedSet.value.has(candidate.path))
    : [entry];
  event.dataTransfer?.setData(
    "application/x-dbx-files",
    JSON.stringify({ paneId: props.paneId ?? "", paths: dragged.map((item) => item.path) }),
  );
  if (event.dataTransfer) event.dataTransfer.effectAllowed = "copyMove";
  emit("drag-entries", { paneId: props.paneId ?? "", entries: dragged });
}

// —— 列自定义（对标 WinSCP/Finder）：表头拖宽 + 列显隐菜单，全局偏好持久化 ——
// 双栏是同一组件的两个实例：任一侧变更经 onColumnPrefsChange 广播即时同步，
// 落盘走 updateColumnPrefs 的读-改-写（不覆盖并发其他键）。
type ColumnKey = "name" | "size" | "modified";

/** 列 key → ColumnPrefs 宽度字段（显式赋值，避免联合键的展开类型歧义）。 */
function withColumnWidth(current: ColumnPrefs, key: ColumnKey, width: number): ColumnPrefs {
  const next = { ...current };
  if (key === "name") next.nameWidth = width;
  else if (key === "size") next.sizeWidth = width;
  else next.modifiedWidth = width;
  return next;
}

/** 列显隐菜单条目；名称列是内容锚点，永远保留（对标 WinSCP 至少一列）。 */
const COLUMN_DEFS: { key: ColumnKey; labelKey: string; locked?: boolean }[] = [
  { key: "name", labelKey: "colName", locked: true },
  { key: "size", labelKey: "colSize" },
  { key: "modified", labelKey: "colModified" },
];

const columns = ref<ColumnPrefs>(loadUiPrefs().columns ?? { ...DEFAULT_COLUMN_PREFS });
const columnMenu = ref<{ x: number; y: number } | null>(null);
const columnMenuEl = ref<HTMLElement>();
const resizingCol = ref<ColumnKey>();

const columnWidths = computed<Record<ColumnKey, number>>(() => ({
  name: columns.value.nameWidth,
  size: columns.value.sizeWidth,
  modified: columns.value.modifiedWidth,
}));

function isColumnHidden(key: ColumnKey) {
  return columns.value.hidden.includes(key);
}

/** 表头/行单元格共用同一套宽度：显式宽 + 允许窄视口收缩，下限对齐列最小宽。 */
function cellStyle(key: ColumnKey) {
  return {
    width: `${columnWidths.value[key]}px`,
    flex: "0 1 auto",
    minWidth: `${COLUMN_MIN_WIDTH[key]}px`,
  };
}

let unsubscribeColumns: (() => void) | undefined;

function onDocumentPointerDown(event: PointerEvent) {
  if (columnMenu.value && columnMenuEl.value && !columnMenuEl.value.contains(event.target as Node)) {
    closeColumnMenu();
  }
}

onMounted(() => {
  unsubscribeColumns = onColumnPrefsChange((next) => {
    columns.value = next;
  });
  document.addEventListener("pointerdown", onDocumentPointerDown, true);
});

onBeforeUnmount(() => {
  unsubscribeColumns?.();
  document.removeEventListener("pointerdown", onDocumentPointerDown, true);
  document.body.classList.remove("wb-col-resizing");
});

// 列显隐菜单：表头右键弹出（沿用 wb-context-menu 视觉），外部按下 / Esc 关闭。
function openColumnMenu(event: MouseEvent) {
  const x = Math.min(event.clientX, Math.max(0, window.innerWidth - 200));
  const y = Math.min(event.clientY, Math.max(0, window.innerHeight - 140));
  columnMenu.value = { x, y };
}

function closeColumnMenu() {
  columnMenu.value = null;
}

function toggleColumn(key: ColumnKey) {
  if (key === "name") return; // 名称列不可隐藏
  const hidden = isColumnHidden(key)
    ? columns.value.hidden.filter((item) => item !== key)
    : [...columns.value.hidden, key];
  columns.value = updateColumnPrefs((current) => ({ ...current, hidden }));
}

// 列宽拖拽：pointerdown 记起点，pointermove 实时调宽（钳制最小值），
// pointerup 落盘；拖拽期间禁用页面文本选择（body.wb-col-resizing）。
interface ResizeState {
  key: ColumnKey;
  startX: number;
  startWidth: number;
  moved: boolean;
}
let resizeState: ResizeState | null = null;

function onResizeStart(key: ColumnKey, event: PointerEvent) {
  if (event.button !== 0) return;
  event.preventDefault();
  event.stopPropagation();
  const el = event.currentTarget as HTMLElement;
  try {
    // Pointer Capture 让指针移出热区后 move/up 仍送达热区；测试环境缺失时静默。
    el.setPointerCapture?.(event.pointerId);
  } catch {
    /* 无 Pointer Capture 环境下同样可用 */
  }
  resizeState = { key, startX: event.clientX, startWidth: columnWidths.value[key], moved: false };
  resizingCol.value = key;
  document.body.classList.add("wb-col-resizing");
}

function onResizeMove(event: PointerEvent) {
  if (!resizeState) return;
  const next = clampColumnWidth(resizeState.startWidth + (event.clientX - resizeState.startX), resizeState.key);
  if (next !== columnWidths.value[resizeState.key]) resizeState.moved = true;
  columns.value = withColumnWidth(columns.value, resizeState.key, next);
}

function onResizeEnd() {
  if (!resizeState) return;
  const { key, moved } = resizeState;
  resizeState = null;
  resizingCol.value = undefined;
  document.body.classList.remove("wb-col-resizing");
  if (!moved) return; // 未动过不落盘
  columns.value = updateColumnPrefs(
    (current) => withColumnWidth(current, key, columnWidths.value[key]),
  );
}
</script>

<template>
  <!-- R3-P2-8：表头 role=row/columnheader + aria-sort（排序方向对屏幕阅读器可感知）。
       表头右键弹出列显隐菜单（对标 WinSCP/Finder）；行内空白区右键仍走 blank-context。 -->
  <div class="wb-file-header" role="row" @contextmenu.prevent.stop="openColumnMenu">
    <!-- 与行首复选框（14px + gap）对齐的占位，保证表头列边界与行单元格对齐。 -->
    <span class="wb-col-check-spacer" aria-hidden="true" />
    <span
      v-if="!isColumnHidden('name')"
      class="wb-col-name"
      role="columnheader"
      :style="cellStyle('name')"
      :aria-sort="sort.column === 'name' ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined"
    >
      <button type="button" @click="emit('sort', 'name')">{{ t("colName") }}<span v-if="sort.column === 'name'" aria-hidden="true">{{ sort.direction === "asc" ? " ↑" : " ↓" }}</span></button>
      <span
        class="wb-col-resize"
        :class="{ 'is-resizing': resizingCol === 'name' }"
        data-test="resize-name"
        aria-hidden="true"
        @pointerdown="onResizeStart('name', $event)"
        @pointermove="onResizeMove"
        @pointerup="onResizeEnd"
        @pointercancel="onResizeEnd"
      />
    </span>
    <span
      v-if="!isColumnHidden('size')"
      class="wb-numeric"
      role="columnheader"
      :style="cellStyle('size')"
      :aria-sort="sort.column === 'size' ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined"
    >
      <button type="button" @click="emit('sort', 'size')">{{ t("colSize") }}<span v-if="sort.column === 'size'" aria-hidden="true">{{ sort.direction === "asc" ? " ↑" : " ↓" }}</span></button>
      <span
        class="wb-col-resize"
        :class="{ 'is-resizing': resizingCol === 'size' }"
        data-test="resize-size"
        aria-hidden="true"
        @pointerdown="onResizeStart('size', $event)"
        @pointermove="onResizeMove"
        @pointerup="onResizeEnd"
        @pointercancel="onResizeEnd"
      />
    </span>
    <span
      v-if="!isColumnHidden('modified')"
      role="columnheader"
      :style="cellStyle('modified')"
      :aria-sort="sort.column === 'modified' ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined"
    >
      <button type="button" @click="emit('sort', 'modified')">{{ t("colModified") }}<span v-if="sort.column === 'modified'" aria-hidden="true">{{ sort.direction === "asc" ? " ↑" : " ↓" }}</span></button>
      <span
        class="wb-col-resize"
        :class="{ 'is-resizing': resizingCol === 'modified' }"
        data-test="resize-modified"
        aria-hidden="true"
        @pointerdown="onResizeStart('modified', $event)"
        @pointermove="onResizeMove"
        @pointerup="onResizeEnd"
        @pointercancel="onResizeEnd"
      />
    </span>
  </div>
  <!-- R3-P2-8：滚动容器 role=listbox + aria-label，行 role=option + aria-selected。 -->
  <div ref="viewport" class="wb-file-scroll" :data-pane-id="paneId ?? ''" role="listbox" aria-multiselectable="true" :aria-label="t('fileListLabel')" :aria-busy="Boolean(loading)" tabindex="0" @scroll="onScroll" @keydown="onListKeydown" @contextmenu.prevent.stop="emit('blank-context', { x: $event.clientX, y: $event.clientY })">
    <div v-if="failed && !loading" class="wb-file-empty" role="status" style="display: flex; flex-direction: column; align-items: center">
      <p>{{ t("directoryLoadFailed") }}</p>
      <button type="button" class="wb-toolbar-button" @click="emit('retry')">{{ t("retry") }}</button>
    </div>
    <div v-else class="wb-file-spacer" :style="{ height: `${totalHeight}px` }">
      <div
        v-for="(entry, localIndex) in visibleEntries"
        v-show="!loading"
        :key="entry.path"
        class="wb-file-row"
        role="option"
        :aria-selected="selectedSet.has(entry.path)"
        :class="{ 'is-selected': selectedSet.has(entry.path), 'is-active': entry.path === activePath }"
        :style="rowStyle(firstIndex + localIndex)"
        draggable="true"
        @click="onClickRow(entry, $event)"
        @dblclick="emit('open', entry)"
        @contextmenu.prevent.stop="onContextRow(entry, $event)"
        @dragstart="onDragStart(entry, $event)"
      >
        <label class="wb-file-check" @click.stop>
          <input
            type="checkbox"
            :checked="selectedSet.has(entry.path)"
            :aria-label="t('selectEntry', { name: entry.displayName ?? entry.name })"
            @change="toggleSelection(entry, { meta: true, shift: false })"
          />
        </label>
        <span class="wb-file-name" :style="cellStyle('name')">
          <Folder v-if="entry.kind === 'directory'" class="wb-icon-dir" />
          <component :is="fileIcon(entry.name)" v-else aria-hidden="true" :class="fileIconClass(entry.name)" />
          <span :title="entry.path">{{ entry.displayName ?? entry.name }}</span>
        </span>
        <span v-if="!isColumnHidden('size')" class="wb-numeric" :style="cellStyle('size')">{{ entry.kind === "directory" ? "" : formatBytes(entry.size) }}</span>
        <span v-if="!isColumnHidden('modified')" class="wb-muted" :style="{ ...cellStyle('modified'), fontSize: '11px' }">{{ formatTime(entry.modifiedAt) }}</span>
      </div>
      <!-- 加载骨架屏（A-FILES ④d）：与行同高的 shimmer 占位 -->
      <template v-if="loading">
        <div v-for="row in SKELETON_ROWS" :key="`skeleton-${row}`" class="wb-file-row" :style="rowStyle(row - 1)">
          <span class="wb-skeleton wb-skeleton-check" />
          <span class="wb-skeleton wb-skeleton-name" :style="{ width: `${30 + ((row * 17) % 40)}%` }" />
          <span class="wb-skeleton wb-skeleton-cell" />
          <span class="wb-skeleton wb-skeleton-cell" />
        </div>
      </template>
      <!-- R3-P2-6：区分「过滤无匹配」与「目录确认为空」两态，避免误导。
           对标 rclone-dashboard empty-state：虚线安静卡片 + 图标。 -->
      <div v-else-if="!entries.length" class="wb-file-empty" role="status">
        <component :is="filtered ? SearchX : FolderOpen" aria-hidden="true" />
        <p>{{ filtered ? t("noMatchResults") : t("emptyDirectory") }}</p>
      </div>
    </div>
  </div>
  <div class="wb-file-footer">
    <span v-if="loading" role="status">{{ t("loading") }}</span>
    <template v-else-if="!failed">
      <span>{{ t("entriesCount", { count: entries.length }) }}</span>
      <span v-if="selection.length">{{ t("selectedCount", { count: selection.length }) }}</span>
      <!-- 批量重命名（parity-tools）：多选时可键盘到达的常驻入口（只读态禁用，
           与 delete/rename 门禁一致）。 -->
      <button
        v-if="selection.length > 1"
        type="button"
        class="wb-toolbar-button"
        data-test="batch-rename"
        :disabled="!canWrite"
        @click="emit('batch-rename')"
      >{{ t("batchRenameMenu") }}</button>
    </template>
  </div>
  <!-- 列显隐菜单：复用 wb-context-menu 视觉；名称列锁定为常显（disabled），
       其余列为 menuitemcheckbox 复选语义。切换后菜单保持打开便于连续调整。 -->
  <div
    v-if="columnMenu"
    ref="columnMenuEl"
    class="wb-context-menu"
    role="menu"
    data-test="column-menu"
    :style="{ left: `${columnMenu.x}px`, top: `${columnMenu.y}px` }"
    @click.stop
    @keydown.esc.stop.prevent="closeColumnMenu"
  >
    <button
      v-for="col in COLUMN_DEFS"
      :key="col.key"
      type="button"
      role="menuitemcheckbox"
      data-test="column-menu-item"
      :aria-checked="!isColumnHidden(col.key)"
      :disabled="col.locked"
      @click="toggleColumn(col.key)"
    >
      <Check v-if="!isColumnHidden(col.key)" aria-hidden="true" />
      <span v-else class="wb-col-menu-gap" aria-hidden="true" />
      {{ t(col.labelKey) }}
    </button>
  </div>
</template>
