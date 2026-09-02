<script setup lang="ts">
// 侧栏导航面板（每栏一个）：tree（目录树，默认）/ quick（快捷目录）双 tab，
// 可收起为窄条再展开；tab 与收缩状态由 App 持久化到 prefs。行右键统一上抛
// node-context（打开 / 在另一栏打开 / 复制路径、文件名），由 App 弹菜单。
import { ChevronsLeft, ChevronsRight, FolderTree, RefreshCw, Star } from "@lucide/vue";
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
    <div class="wb-side-body">
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
