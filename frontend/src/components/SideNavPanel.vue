<script setup lang="ts">
// 侧栏导航面板（每栏一个）：tree（目录树，默认）/ quick（快捷目录）/ fav
// （收藏夹）三 tab，可收起为窄条再展开；tab 与收缩状态由 App 持久化到 prefs。
// 行右键统一上抛 node-context（打开 / 在另一栏打开 / 收藏切换 / 复制路径、
// 文件名），由 App 弹菜单；fav 行右键同走此通道（App 端按已收藏态切文案）。
import { ChevronsLeft, ChevronsRight, FolderOpen, FolderTree, RefreshCw, Star, Zap } from "@lucide/vue";
import { ref } from "vue";
import DirTree from "./DirTree.vue";
import type { DirTreeNode } from "../lib/dirTree";
import { quickPathIcon, quickPathLabelKey, type QuickPath } from "../lib/quickPaths";
import { baseName, formatBytes } from "../lib/api";
import { DEFAULT_SIDEBAR_WIDTH, clampSidebarWidth } from "../lib/prefs";

const props = defineProps<{
  side: "left" | "right";
  tab: "tree" | "quick" | "fav";
  collapsed: boolean;
  /** 侧栏宽度（px）：App 单点持有并持久化（双栏共用一份），缺省走默认宽。 */
  width?: number;
  treeRoot: DirTreeNode | null;
  quickPaths: QuickPath[];
  /** 当前连接已收藏的目录路径（App 按 connectionId 键入后下发）。 */
  favorites: string[];
  currentPath: string;
  t: (key: string, values?: Record<string, string | number>) => string;
  /** 远端空间占用（files/about，60s sidecar 缓存；App 按栏传入：左栏=当前
   *  连接，右栏=目标连接；本地 __local__ 或后端不支持时为 null 隐藏）。 */
  usage?: { used: number; total: number } | null;
}>();

const emit = defineEmits<{
  (event: "update:tab", tab: "tree" | "quick" | "fav"): void;
  (event: "update:collapsed", collapsed: boolean): void;
  /** issue #90：拖拽过程中实时上抛（父组件驱动双栏即时同步），不落盘。 */
  (event: "update:width", width: number): void;
  /** 松手（或双击重置）后上抛，父组件统一落盘。 */
  (event: "width-commit"): void;
  (event: "navigate", path: string): void;
  (event: "toggle-node", node: DirTreeNode): void;
  (event: "refresh-tree"): void;
  (event: "node-context", payload: { path: string; name: string; x: number; y: number }): void;
}>();

/** 当前生效宽度：prop 缺失（未接父组件）时按默认宽渲染。 */
function panelWidth(): number {
  return clampSidebarWidth(props.width ?? DEFAULT_SIDEBAR_WIDTH);
}

// —— 侧栏拖宽（issue #90 分栏没法调整）：右缘 6px 手柄，pointer capture 实时
// 调宽（钳制上下限），松手上抛 commit 由 App 落盘；双击重置默认宽。
// 手柄 aria-hidden 与 wb-col-resize 同口径（纯指针交互）。 ——
const sideResizing = ref(false);
let sideResizeState: { startX: number; startWidth: number; moved: boolean } | null = null;

function onSideResizeStart(event: PointerEvent) {
  if (event.button !== 0) return;
  event.preventDefault();
  const el = event.currentTarget as HTMLElement;
  try {
    // Pointer Capture 让指针移出热区后 move/up 仍送达手柄；测试环境缺失时静默。
    el.setPointerCapture?.(event.pointerId);
  } catch {
    /* 无 Pointer Capture 环境下同样可用 */
  }
  sideResizeState = { startX: event.clientX, startWidth: panelWidth(), moved: false };
  sideResizing.value = true;
  document.body.classList.add("wb-side-resizing");
}

function onSideResizeMove(event: PointerEvent) {
  if (!sideResizeState) return;
  const next = clampSidebarWidth(sideResizeState.startWidth + (event.clientX - sideResizeState.startX));
  if (next !== sideResizeState.startWidth) sideResizeState.moved = true;
  emit("update:width", next);
}

function onSideResizeEnd() {
  if (!sideResizeState) return;
  const { moved } = sideResizeState;
  sideResizeState = null;
  sideResizing.value = false;
  document.body.classList.remove("wb-side-resizing");
  if (moved) emit("width-commit");
}

function onSideResizeReset() {
  emit("update:width", DEFAULT_SIDEBAR_WIDTH);
  emit("width-commit");
}

function displayName(name: string): string {
  return name === "/" ? props.t("quickRoot") : name;
}

function onTreeContext(payload: { node: DirTreeNode; x: number; y: number }) {
  emit("node-context", { path: payload.node.path, name: displayName(payload.node.name), x: payload.x, y: payload.y });
}

function onFavContext(path: string, x: number, y: number) {
  emit("node-context", { path, name: baseName(path) || path, x, y });
}

// P2-9 目录树键盘可达：容器 tabindex=0 + roving focus——树行（wb-tree-row）
// 本身 tabindex=-1，↑↓ 在「已挂载行」间移动焦点，Enter/Space 打开（进入目录），
// ←/→ 收起/展开（点击行内 caret）；focus 落在 caret 按钮等子控件时优先自愈到行。
// fav 行（wb-fav-row）复用同一 roving 模式（无 caret，←/→ 不处理）。
const treeBody = ref<HTMLElement>();

function rowSelector(): string {
  return props.tab === "fav" ? ".wb-fav-row" : ".wb-tree-row";
}

function focusedTreeRow(rows: HTMLElement[]): HTMLElement | null {
  const active = document.activeElement;
  return active instanceof HTMLElement && rows.includes(active) ? active : null;
}

function onTreeKeydown(event: KeyboardEvent) {
  if (props.tab !== "tree" && props.tab !== "fav") return;
  const body = treeBody.value;
  if (!body) return;
  const rows = Array.from(body.querySelectorAll<HTMLElement>(rowSelector()));
  if (!rows.length) return;
  const row = focusedTreeRow(rows);
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const from = row ? rows.indexOf(row) : -1;
    const next = from < 0 ? (event.key === "ArrowDown" ? 0 : rows.length - 1) : event.key === "ArrowDown" ? Math.min(rows.length - 1, from + 1) : Math.max(0, from - 1);
    rows[next]?.focus();
    return;
  }
  if (!row) return;
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    row.click(); // 行 click 语义 = 本栏进入该目录（与鼠标单击一致）
    return;
  }
  if (props.tab === "tree" && (event.key === "ArrowRight" || event.key === "ArrowLeft")) {
    event.preventDefault();
    row.querySelector<HTMLButtonElement>(".wb-tree-caret")?.click();
  }
}
</script>

<template>
  <div v-if="!collapsed" class="wb-side-panel" :style="{ flex: `0 0 ${panelWidth()}px`, width: `${panelWidth()}px` }">
    <div v-if="usage && usage.total > 0" class="wb-side-usage" :title="t('sideUsage')">
      <div class="wb-side-usage-bar"><div class="wb-side-usage-fill" :style="{ width: `${Math.min(100, Math.round((usage.used / usage.total) * 100))}%` }" /></div>
      <span class="wb-muted">{{ t("sideUsage") }}: {{ formatBytes(usage.used) }} / {{ formatBytes(usage.total) }}</span>
    </div>
    <div class="wb-side-tabs">
      <button type="button" :class="{ 'is-active': tab === 'tree' }" v-tip="t('sideTree')" @click="emit('update:tab', 'tree')">
        <FolderTree />
      </button>
      <button type="button" :class="{ 'is-active': tab === 'quick' }" v-tip="t('quickPathsTitle')" @click="emit('update:tab', 'quick')">
        <Zap />
      </button>
      <button type="button" :class="{ 'is-active': tab === 'fav' }" v-tip="t('favTitle')" @click="emit('update:tab', 'fav')">
        <Star />
      </button>
      <span class="wb-side-spacer" />
      <button v-if="tab === 'tree'" type="button" v-tip="t('refresh')" @click="emit('refresh-tree')">
        <RefreshCw />
      </button>
      <button type="button" v-tip="t('sideCollapse')" @click="emit('update:collapsed', true)">
        <ChevronsLeft />
      </button>
    </div>
    <div
      ref="treeBody"
      class="wb-side-body"
      :tabindex="tab === 'tree' || tab === 'fav' ? 0 : undefined"
      :role="tab === 'tree' ? 'tree' : undefined"
      :aria-label="tab === 'fav' ? t('favTitle') : t('sideTree')"
      @keydown="onTreeKeydown"
    >
      <DirTree
        v-if="tab === 'tree' && treeRoot"
        :nodes="[treeRoot]"
        :depth="0"
        :current-path="currentPath"
        :t="t"
        @toggle="emit('toggle-node', $event)"
        @open="emit('navigate', $event.path)"
        @context="onTreeContext"
      />
      <div v-else-if="tab === 'quick'" class="wb-side-quick">
        <div v-if="!quickPaths.length" class="wb-side-empty">{{ t("quickEmpty") }}</div>
        <button
          v-for="qp in quickPaths"
          :key="qp.key"
          type="button"
          :class="{ 'is-current': qp.path === currentPath }"
          :title="qp.path"
          @click="emit('navigate', qp.path)"
          @contextmenu.prevent.stop="emit('node-context', { path: qp.path, name: t(quickPathLabelKey(qp.key)), x: $event.clientX, y: $event.clientY })"
        >
          <component :is="quickPathIcon(qp.key)" aria-hidden="true" />
          <span>{{ t(quickPathLabelKey(qp.key)) }}</span>
        </button>
      </div>
      <!-- 收藏夹：当前连接星标目录；点击进入，右键走统一侧栏菜单（移除/复制路径）。 -->
      <div v-else-if="tab === 'fav'" class="wb-side-quick">
        <div v-if="!favorites.length" class="wb-side-empty">{{ t("favEmpty") }}</div>
        <button
          v-for="favPath in favorites"
          :key="favPath"
          type="button"
          class="wb-fav-row"
          :class="{ 'is-current': favPath === currentPath }"
          :tabindex="-1"
          :title="favPath"
          @click="emit('navigate', favPath)"
          @contextmenu.prevent.stop="onFavContext(favPath, $event.clientX, $event.clientY)"
        >
          <FolderOpen aria-hidden="true" />
          <span>{{ favPath === "/" ? t("quickRoot") : favPath }}</span>
        </button>
      </div>
    </div>
    <span
      class="wb-side-resize"
      :class="{ 'is-resizing': sideResizing }"
      data-test="side-resize"
      aria-hidden="true"
      @pointerdown="onSideResizeStart"
      @pointermove="onSideResizeMove"
      @pointerup="onSideResizeEnd"
      @pointercancel="onSideResizeEnd"
      @dblclick="onSideResizeReset"
    />
  </div>
  <div v-else class="wb-side-rail">
    <button type="button" v-tip="t('sideExpand')" @click="emit('update:collapsed', false)">
      <ChevronsRight />
    </button>
  </div>
</template>
