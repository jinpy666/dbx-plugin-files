<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { Folder, FolderOpen, SearchX } from "@lucide/vue";
import { formatBytes, formatTime, type FileEntry } from "../lib/api";
import { fileIcon, fileIconClass } from "../lib/fileIcons";
import { listNav, scrollRowIntoView, selectionRange, type ListNavState } from "../lib/listNav";

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
</script>

<template>
  <!-- R3-P2-8：表头 role=row/columnheader + aria-sort（排序方向对屏幕阅读器可感知）。 -->
  <div class="wb-file-header" role="row" @contextmenu.prevent.stop="emit('blank-context', { x: $event.clientX, y: $event.clientY })">
    <span class="wb-col-name" role="columnheader" :aria-sort="sort.column === 'name' ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined">
      <button type="button" @click="emit('sort', 'name')">{{ t("colName") }}<span v-if="sort.column === 'name'" aria-hidden="true">{{ sort.direction === "asc" ? " ↑" : " ↓" }}</span></button>
    </span>
    <span class="wb-numeric" style="width: 90px" role="columnheader" :aria-sort="sort.column === 'size' ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined">
      <button type="button" @click="emit('sort', 'size')">{{ t("colSize") }}<span v-if="sort.column === 'size'" aria-hidden="true">{{ sort.direction === "asc" ? " ↑" : " ↓" }}</span></button>
    </span>
    <span style="width: 130px" role="columnheader" :aria-sort="sort.column === 'modified' ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined">
      <button type="button" @click="emit('sort', 'modified')">{{ t("colModified") }}<span v-if="sort.column === 'modified'" aria-hidden="true">{{ sort.direction === "asc" ? " ↑" : " ↓" }}</span></button>
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
            :aria-label="t('selectEntry', { name: entry.name })"
            @change="toggleSelection(entry, { meta: true, shift: false })"
          />
        </label>
        <span class="wb-file-name">
          <Folder v-if="entry.kind === 'directory'" class="wb-icon-dir" />
          <component :is="fileIcon(entry.name)" v-else aria-hidden="true" :class="fileIconClass(entry.name)" />
          <span :title="entry.path">{{ entry.name }}</span>
        </span>
        <span class="wb-numeric" style="width: 90px">{{ entry.kind === "directory" ? "" : formatBytes(entry.size) }}</span>
        <span class="wb-muted" style="width: 130px; font-size: 11px">{{ formatTime(entry.modifiedAt) }}</span>
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
</template>
