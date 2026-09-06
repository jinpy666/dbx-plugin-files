<script setup lang="ts">
// 侧栏导航面板（每栏一个）：tree（目录树，默认）/ quick（快捷目录）双 tab，
// 可收起为窄条再展开；tab 与收缩状态由 App 持久化到 prefs。行右键统一上抛
// node-context（打开 / 在另一栏打开 / 复制路径、文件名），由 App 弹菜单。
import { ChevronsLeft, ChevronsRight, FolderTree, RefreshCw, Star } from "@lucide/vue";
import { ref } from "vue";
import DirTree from "./DirTree.vue";
import type { DirTreeNode } from "../lib/dirTree";
import { quickPathIcon, quickPathLabelKey, type QuickPath } from "../lib/quickPaths";

const props = defineProps<{
  side: "left" | "right";
  tab: "tree" | "quick";
  collapsed: boolean;
  treeRoot: DirTreeNode | null;
  quickPaths: QuickPath[];
  currentPath: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "update:tab", tab: "tree" | "quick"): void;
  (event: "update:collapsed", collapsed: boolean): void;
  (event: "navigate", path: string): void;
  (event: "toggle-node", node: DirTreeNode): void;
  (event: "refresh-tree"): void;
  (event: "node-context", payload: { path: string; name: string; x: number; y: number }): void;
}>();

function displayName(name: string): string {
  return name === "/" ? props.t("quickRoot") : name;
}

function onTreeContext(payload: { node: DirTreeNode; x: number; y: number }) {
  emit("node-context", { path: payload.node.path, name: displayName(payload.node.name), x: payload.x, y: payload.y });
}

// P2-9 目录树键盘可达：容器 tabindex=0 + roving focus——树行（wb-tree-row）
// 本身 tabindex=-1，↑↓ 在「已挂载行」间移动焦点，Enter/Space 打开（进入目录），
// ←/→ 收起/展开（点击行内 caret）；focus 落在 caret 按钮等子控件时优先自愈到行。
const treeBody = ref<HTMLElement>();

function focusedTreeRow(rows: HTMLElement[]): HTMLElement | null {
  const active = document.activeElement;
  return active instanceof HTMLElement && rows.includes(active) ? active : null;
}

function onTreeKeydown(event: KeyboardEvent) {
  if (props.tab !== "tree") return;
  const body = treeBody.value;
  if (!body) return;
  const rows = Array.from(body.querySelectorAll<HTMLElement>(".wb-tree-row"));
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
  if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
    event.preventDefault();
    row.querySelector<HTMLButtonElement>(".wb-tree-caret")?.click();
  }
}
</script>

<template>
  <div v-if="!collapsed" class="wb-side-panel">
    <div class="wb-side-tabs">
      <button type="button" :class="{ 'is-active': tab === 'tree' }" :title="t('sideTree')" @click="emit('update:tab', 'tree')">
        <FolderTree />
      </button>
      <button type="button" :class="{ 'is-active': tab === 'quick' }" :title="t('quickPathsTitle')" @click="emit('update:tab', 'quick')">
        <Star />
      </button>
      <span class="wb-side-spacer" />
      <button v-if="tab === 'tree'" type="button" :title="t('refresh')" @click="emit('refresh-tree')">
        <RefreshCw />
      </button>
      <button type="button" :title="t('sideCollapse')" @click="emit('update:collapsed', true)">
        <ChevronsLeft />
      </button>
    </div>
    <div
      ref="treeBody"
      class="wb-side-body"
      :tabindex="tab === 'tree' ? 0 : undefined"
      role="tree"
      :aria-label="t('sideTree')"
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
    </div>
  </div>
  <div v-else class="wb-side-rail">
    <button type="button" :title="t('sideExpand')" @click="emit('update:collapsed', false)">
      <ChevronsRight />
    </button>
  </div>
</template>
