<script setup lang="ts">
// 跨栏拖放动作选择（rclone-ui parity）：内部拖放（x-dbx-files）落到目标栏后
// 先选「复制（保留原件）/ 移动（传输后删除原件）」，确认后才走 App 既有
// copy/move 链路（冲突预检/覆盖确认仍在 App 层，不在此重复）。默认复制；
// ↑↓/←→ 在两个选项间切换并同步焦点，Enter 确认当前选择，Esc 取消；
// Tab 焦点陷阱复用 lib/a11y trapTabKey，关闭后焦点归还打开前的触发元素。
import { nextTick, ref, watch } from "vue";
import { Copy, FolderInput } from "@lucide/vue";
import { trapTabKey } from "../lib/a11y";

const props = defineProps<{
  open: boolean;
  /** 拖放目标目录（展示用；执行仍由 App 按提交时刻的栏位状态解析）。 */
  targetPath: string;
  count: number;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "choose", action: "copy" | "move"): void;
  (event: "cancel"): void;
}>();

const dialogEl = ref<HTMLElement>();
/** 选中的动作：默认复制（保留原件），与旧行为一致。 */
const selected = ref<"copy" | "move">("copy");
let returnFocusTo: HTMLElement | null = null;

watch(
  () => props.open,
  async (open) => {
    if (open) {
      selected.value = "copy";
      returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      await nextTick();
      if (!props.open) return; // 同帧内又被关闭：不抢焦点
      dialogEl.value?.querySelector<HTMLElement>(".wb-dropaction-option")?.focus();
      return;
    }
    returnFocusTo?.focus();
    returnFocusTo = null;
  },
  // 挂载时即 open（测试/热重载）也要完成聚焦链路
  { immediate: true },
);

/** ↑↓/←→ 在两选项间切换（选择与焦点同步移动）；Enter 仅在选项上确认当前
 * 选择（底部按钮走原生 click）；Esc 取消；其余 Tab 交给焦点陷阱。 */
function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") {
    event.preventDefault();
    emit("cancel");
    return;
  }
  if (event.key === "ArrowDown" || event.key === "ArrowUp" || event.key === "ArrowLeft" || event.key === "ArrowRight") {
    event.preventDefault();
    selected.value = selected.value === "copy" ? "move" : "copy";
    // 焦点按序号定位（类名经 Vue 调度器异步刷新，同步 query 不可靠）。
    const rows = [...(dialogEl.value?.querySelectorAll<HTMLElement>(".wb-dropaction-option") ?? [])];
    (selected.value === "copy" ? rows[0] : rows[1])?.focus();
    return;
  }
  if (event.key === "Enter") {
    const target = event.target;
    if (!(target instanceof HTMLElement) || !target.classList.contains("wb-dropaction-option")) return;
    event.preventDefault();
    emit("choose", selected.value);
    return;
  }
  trapTabKey(event, dialogEl.value);
}
</script>

<template>
  <div v-if="open" class="wb-dialog-backdrop" @click.self="emit('cancel')">
    <div ref="dialogEl" class="wb-dialog" role="dialog" aria-modal="true" :aria-label="t('dropActionTitle')" @keydown="onKeydown">
      <header>{{ t("dropActionTitle") }}</header>
      <div class="wb-dialog-body">
        <p class="wb-dropaction-meta">{{ t("dropActionItems", { count }) }}</p>
        <p class="wb-dropaction-meta">{{ t("dropActionTarget", { path: targetPath }) }}</p>
        <div class="wb-dropaction-options" role="radiogroup" :aria-label="t('dropActionTitle')">
          <button
            type="button"
            role="radio"
            class="wb-dropaction-option"
            :class="{ 'is-active': selected === 'copy' }"
            :aria-checked="selected === 'copy'"
            @click="selected = 'copy'"
          >
            <Copy aria-hidden="true" />
            <span>{{ t("dropActionCopy") }}</span>
          </button>
          <button
            type="button"
            role="radio"
            class="wb-dropaction-option"
            :class="{ 'is-active': selected === 'move' }"
            :aria-checked="selected === 'move'"
            @click="selected = 'move'"
          >
            <FolderInput aria-hidden="true" />
            <span>{{ t("dropActionMove") }}</span>
          </button>
        </div>
      </div>
      <footer>
        <button type="button" class="wb-dialog-cancel" @click="emit('cancel')">{{ t("cancel") }}</button>
        <button type="button" class="wb-dialog-primary" @click="emit('choose', selected)">{{ t("confirm") }}</button>
      </footer>
    </div>
  </div>
</template>
