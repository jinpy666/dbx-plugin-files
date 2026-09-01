<script setup lang="ts">
// 快速定位侧栏（文件管理器对标）：栏内左侧固定一列高频目录（根/主目录/桌面/
// 下载/文档/图片，§8.1 quickPaths，逐项经后端 stat 校验），点击直达；当前
// 所在目录高亮。仅当 quickPaths 多于 root 一项时由宿主（App.vue）挂载。
import { quickPathIcon, quickPathLabelKey, type QuickPath } from "../lib/quickPaths";

const props = defineProps<{
  paths: QuickPath[];
  currentPath: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "navigate", path: string): void;
}>();

function label(key: string): string {
  return props.t(quickPathLabelKey(key));
}
</script>

<template>
  <aside class="wb-quick-sidebar">
    <div class="wb-quick-sidebar-title">{{ t("quickPathsTitle") }}</div>
    <button
      v-for="qp in paths"
      :key="qp.key"
      type="button"
      :class="{ 'is-current': qp.path === currentPath }"
      :title="qp.path"
      @click="emit('navigate', qp.path)"
    >
      <component :is="quickPathIcon(qp.key)" aria-hidden="true" />
      <span>{{ label(qp.key) }}</span>
    </button>
  </aside>
</template>
