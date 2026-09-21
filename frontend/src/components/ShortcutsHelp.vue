<script setup lang="ts">
// 快捷键速查弹层（parity-tools）：汇总文件浏览器全部键盘快捷键，按
// 浏览/选择/操作分组。`?` 触发（App 全局 keydown）、Esc/遮罩/关闭钮关闭；
// 焦点陷阱沿用 ConfirmDialog 的 trapTabKey 方案。
import { nextTick, onMounted, ref } from "vue";
import { X } from "@lucide/vue";
import { trapTabKey } from "../lib/a11y";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
}>();

const overlayEl = ref<HTMLElement>();

interface ShortcutItem {
  keys: string;
  labelKey: string;
}

interface ShortcutGroup {
  titleKey: string;
  items: ShortcutItem[];
}

// 与 FileTable 列表键处理 + App 全局键一一对应（清单与实现同步维护）。
const groups: ShortcutGroup[] = [
  {
    titleKey: "shortcutsGroupBrowse",
    items: [
      { keys: "↑ ↓", labelKey: "scArrowNav" },
      { keys: "Home End", labelKey: "scHomeEnd" },
      { keys: "?", labelKey: "scQuestionHelp" },
      { keys: "Esc", labelKey: "scEscClose" },
    ],
  },
  {
    titleKey: "shortcutsGroupSelect",
    items: [
      { keys: "Shift + ↑ ↓", labelKey: "scShiftArrow" },
      { keys: "Space", labelKey: "scSpaceToggle" },
      { keys: "Cmd/Ctrl + A", labelKey: "scSelectAll" },
    ],
  },
  {
    titleKey: "shortcutsGroupActions",
    items: [
      { keys: "Enter", labelKey: "scEnterOpen" },
      { keys: "F2", labelKey: "scF2Rename" },
      { keys: "Delete", labelKey: "scDeleteKey" },
    ],
  },
];

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") {
    event.preventDefault();
    emit("close");
    return;
  }
  if (event.key === "Tab") trapTabKey(event, overlayEl.value);
}

onMounted(async () => {
  await nextTick();
  overlayEl.value?.focus();
});
</script>

<template>
  <div ref="overlayEl" class="wb-dialog-backdrop" tabindex="-1" @click.self="emit('close')" @keydown="onKeydown">
    <div class="wb-dialog wb-shortcuts" role="dialog" aria-modal="true" :aria-label="props.t('shortcutsTitle')">
      <header>
        <strong>{{ props.t("shortcutsTitle") }}</strong>
        <button type="button" class="wb-icon-button wb-icon-neutral" data-test="shortcuts-close" :aria-label="props.t('close')" @click="emit('close')"><X /></button>
      </header>
      <div class="wb-shortcuts-body" data-test="shortcuts-body">
        <template v-for="group in groups" :key="group.titleKey">
          <div class="wb-shortcuts-group">{{ props.t(group.titleKey) }}</div>
          <ul class="wb-shortcuts-list">
            <li v-for="item in group.items" :key="item.labelKey">
              <span>{{ props.t(item.labelKey) }}</span>
              <kbd class="wb-shortcuts-key">{{ item.keys }}</kbd>
            </li>
          </ul>
        </template>
      </div>
    </div>
  </div>
</template>
