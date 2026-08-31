<script setup lang="ts">
// 路径栏快速目录下拉（tiny-rdm 对标，替代早期 chips 行）：点击触发按钮弹出
// 常用目录菜单（根目录 + fs 协议的用户目录族），两侧栏共用。
import { onBeforeUnmount, onMounted, ref, type Component } from "vue";
import { ChevronDown, Download, FileText, HardDrive, Home, Image as ImageIcon, Monitor } from "@lucide/vue";
import { quickPathLabelKey, type QuickPath } from "../lib/quickPaths";

const props = defineProps<{
  paths: QuickPath[];
  currentPath: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "navigate", path: string): void;
}>();

const ICONS: Record<string, Component> = {
  root: HardDrive,
  home: Home,
  desktop: Monitor,
  downloads: Download,
  documents: FileText,
  pictures: ImageIcon,
};

const open = ref(false);
const rootEl = ref<HTMLElement>();

function label(key: string): string {
  return props.t(quickPathLabelKey(key));
}

function pick(target: QuickPath) {
  open.value = false;
  emit("navigate", target.path);
}

function onDocumentClick(event: MouseEvent) {
  if (open.value && rootEl.value && !rootEl.value.contains(event.target as Node)) open.value = false;
}

function onDocumentKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") open.value = false;
}

onMounted(() => {
  document.addEventListener("click", onDocumentClick);
  document.addEventListener("keydown", onDocumentKeydown);
});
onBeforeUnmount(() => {
  document.removeEventListener("click", onDocumentClick);
  document.removeEventListener("keydown", onDocumentKeydown);
});
</script>

<template>
  <span ref="rootEl" class="wb-quick-menu" :class="{ 'is-open': open }">
    <button
      type="button"
      class="wb-icon-button wb-quick-trigger"
      :title="t('quickPathsTitle')"
      :aria-expanded="open"
      @click="open = !open"
    >
      <ChevronDown aria-hidden="true" />
    </button>
    <div v-if="open" class="wb-quick-menu-pop" role="menu">
      <button
        v-for="qp in paths"
        :key="qp.key"
        type="button"
        role="menuitem"
        :class="{ 'is-current': qp.path === currentPath }"
        :title="qp.path"
        @click="pick(qp)"
      >
        <component :is="ICONS[qp.key]" aria-hidden="true" />
        <span>{{ label(qp.key) }}</span>
      </button>
    </div>
  </span>
</template>
