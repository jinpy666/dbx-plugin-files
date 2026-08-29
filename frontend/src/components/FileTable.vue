<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { File, Folder } from "@lucide/vue";
import { formatBytes, formatTime, type FileEntry } from "../lib/api";

const props = defineProps<{
  entries: FileEntry[];
  selection: string[];
  activePath: string;
  sort: { column: "name" | "size" | "modified"; direction: "asc" | "desc" };
  loading?: boolean;
  /** 双栏拖拽标识：写入 dataTransfer，接收端据此判断来源与目标。 */
  paneId?: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "update:selection", value: string[]): void;
  (event: "update:activePath", value: string): void;
  (event: "open", entry: FileEntry): void;
  (event: "contextmenu", payload: { entry: FileEntry; x: number; y: number }): void;
  (event: "sort", column: "name" | "size" | "modified"): void;
  /** A-FILES ①：行拖拽开始，携带拖拽集（已选中项或单行）。 */
  (event: "drag-entries", payload: { paneId: string; entries: FileEntry[] }): void;
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
    if (viewport.value) viewport.value.scrollTop = 0;
  },
);

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
  <div class="wb-file-header">
    <span class="wb-col-name">
      <button type="button" @click="emit('sort', 'name')">{{ t("colName") }}{{ sort.column === "name" ? (sort.direction === "asc" ? " ↑" : " ↓") : "" }}</button>
    </span>
    <span class="wb-numeric" style="width: 90px">
      <button type="button" @click="emit('sort', 'size')">{{ t("colSize") }}{{ sort.column === "size" ? (sort.direction === "asc" ? " ↑" : " ↓") : "" }}</button>
    </span>
    <span style="width: 130px">
      <button type="button" @click="emit('sort', 'modified')">{{ t("colModified") }}{{ sort.column === "modified" ? (sort.direction === "asc" ? " ↑" : " ↓") : "" }}</button>
    </span>
  </div>
  <div ref="viewport" class="wb-file-scroll" @scroll="onScroll">
    <div class="wb-file-spacer" :style="{ height: `${totalHeight}px` }">
      <div
        v-for="(entry, localIndex) in visibleEntries"
        v-show="!loading"
        :key="entry.path"
        class="wb-file-row"
        :class="{ 'is-selected': selectedSet.has(entry.path), 'is-active': entry.path === activePath }"
        :style="rowStyle(firstIndex + localIndex)"
        draggable="true"
        @click="onClickRow(entry, $event)"
        @dblclick="emit('open', entry)"
        @contextmenu.prevent="onContextRow(entry, $event)"
        @dragstart="onDragStart(entry, $event)"
      >
        <label class="wb-file-check" @click.stop>
          <input
            type="checkbox"
            :checked="selectedSet.has(entry.path)"
            @change="toggleSelection(entry, { meta: true, shift: false })"
          />
        </label>
        <span class="wb-file-name">
          <Folder v-if="entry.kind === 'directory'" class="wb-icon-dir" />
          <File v-else />
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
      <div v-else-if="!entries.length" class="wb-file-empty">{{ t("emptyDirectory") }}</div>
    </div>
  </div>
  <div class="wb-file-footer">
    <span>{{ t("entriesCount", { count: entries.length }) }}</span>
    <span v-if="selection.length">{{ t("selectedCount", { count: selection.length }) }}</span>
  </div>
</template>
