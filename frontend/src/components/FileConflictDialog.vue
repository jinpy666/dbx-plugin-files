<script setup lang="ts">
// 传输同名冲突三键弹窗（对标 ssh 插件 DownloadConflictPrompt）：上传/下载
// 预检发现撞名时询问「覆盖 / 自动重命名 / 取消」。焦点管理与 ConfirmDialog
// 同款（首焦点落安全项「取消」，防 Enter 误覆盖；Tab 循环；关闭归还焦点）。
import { nextTick, ref, watch } from "vue";

const props = defineProps<{
  open: boolean;
  title: string;
  /** 正文（i18n 已含数量与文件名列表，由父层拼好）。 */
  message: string;
  overwriteLabel: string;
  renameLabel: string;
  cancelLabel: string;
}>();

const emit = defineEmits<{
  (event: "choose", mode: "overwrite" | "rename"): void;
  (event: "cancel"): void;
}>();

const dialogEl = ref<HTMLElement>();
let returnFocusTo: HTMLElement | null = null;

const FOCUSABLE_SELECTOR = "button:not([disabled])";

watch(
  () => props.open,
  async (open) => {
    if (open) {
      returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      await nextTick();
      if (!props.open) return;
      dialogEl.value?.querySelector<HTMLElement>(".wb-dialog-cancel")?.focus();
      return;
    }
    returnFocusTo?.focus();
    returnFocusTo = null;
  },
  { immediate: true },
);

function onTabKeydown(event: KeyboardEvent) {
  if (event.key !== "Tab") return;
  const root = dialogEl.value;
  if (!root) return;
  const focusables = [...root.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)];
  if (!focusables.length) return;
  const first = focusables[0]!;
  const last = focusables[focusables.length - 1]!;
  const current = document.activeElement;
  const inside = current instanceof HTMLElement && root.contains(current);
  if (event.shiftKey) {
    if (!inside || current === first) {
      event.preventDefault();
      last.focus();
    }
    return;
  }
  if (!inside || current === last) {
    event.preventDefault();
    first.focus();
  }
}
</script>

<template>
  <div v-if="open" class="wb-dialog-backdrop" @click.self="emit('cancel')">
    <div ref="dialogEl" class="wb-dialog" role="dialog" aria-modal="true" @keydown="onTabKeydown">
      <header>{{ title }}</header>
      <div class="wb-dialog-body">
        <p style="margin: 0">{{ message }}</p>
      </div>
      <footer>
        <button type="button" class="wb-dialog-cancel" @click="emit('cancel')">{{ cancelLabel }}</button>
        <button type="button" class="wb-dialog-danger" @click="emit('choose', 'overwrite')">{{ overwriteLabel }}</button>
        <button type="button" class="wb-dialog-primary" @click="emit('choose', 'rename')">{{ renameLabel }}</button>
      </footer>
    </div>
  </div>
</template>
