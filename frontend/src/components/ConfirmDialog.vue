<script setup lang="ts">
// 确认弹层（P1-3 焦点管理）：
// ① 打开即聚焦首控件——有表单输入直落输入框（全选便于改名），否则聚焦
//    安全项（取消钮），危险操作防 Enter 误确认；
// ② Tab/Shift+Tab 在弹层内循环（focus trap），焦点不再跑出弹层触发背景按钮；
// ③ 关闭（确认/取消/Esc）后焦点归还打开前的触发元素。
import { nextTick, ref, watch } from "vue";

const props = defineProps<{
  open: boolean;
  title: string;
  body?: string;
  danger?: boolean;
  dangerList?: string[];
  /** R3-P2-4：行内校验提示（如文件名非法），显示在 slot 表单下方。 */
  warning?: string;
  confirmLabel: string;
  cancelLabel: string;
  busy?: boolean;
  /** 有 slot 内容（表单）时确认按钮的可用性由父组件控制。 */
  confirmDisabled?: boolean;
}>();

const emit = defineEmits<{
  (event: "confirm"): void;
  (event: "cancel"): void;
}>();

const dialogEl = ref<HTMLElement>();
/** 打开前的 document.activeElement，关闭时归还焦点。 */
let returnFocusTo: HTMLElement | null = null;

const FOCUSABLE_SELECTOR = 'button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])';

watch(
  () => props.open,
  async (open) => {
    if (open) {
      returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      await nextTick();
      if (!props.open) return; // 同帧内又被关闭（如连续 Esc）：不抢焦点
      const root = dialogEl.value;
      if (!root) return;
      const input = root.querySelector<HTMLInputElement>("input, textarea");
      // 审计中#9：danger 确认（syncDir/copyDir 等）首焦点不落输入框——输入框
      // 聚焦 + Enter 会一步触发危险动作；改落取消钮（安全项），输入需先 Tab。
      if (props.danger) {
        (root.querySelector<HTMLElement>(".wb-dialog-cancel") ?? input)?.focus();
        return;
      }
      input?.focus();
      input?.select();
      if (!input) root.querySelector<HTMLElement>(".wb-dialog-cancel")?.focus();
      return;
    }
    returnFocusTo?.focus();
    returnFocusTo = null;
  },
  // 挂载时即 open（如测试或热重载场景）也要完成聚焦链路
  { immediate: true },
);

/** Tab 焦点陷阱：焦点已末位时 Tab 回绕到首位，Shift+Tab 反向；焦点意外
 * 落在弹层外（BODY）时也拉回弹层内。 */
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
    <div ref="dialogEl" class="wb-dialog" :class="{ 'is-danger': danger }" role="dialog" aria-modal="true" @keydown="onTabKeydown">
      <header>{{ title }}</header>
      <div class="wb-dialog-body">
        <p v-if="body" style="margin: 0 0 6px">{{ body }}</p>
        <slot />
        <!-- R3-P2-4：文件名等行内校验提示（role=alert 及时播报）。 -->
        <p v-if="warning" class="wb-dialog-warning" role="alert">{{ warning }}</p>
        <ul v-if="dangerList?.length" class="wb-dialog-danger-list">
          <li v-for="hit in dangerList" :key="hit">{{ hit }}</li>
        </ul>
      </div>
      <footer>
        <button type="button" class="wb-dialog-cancel" :disabled="busy" @click="emit('cancel')">{{ cancelLabel }}</button>
        <button type="button" :class="danger ? 'wb-dialog-danger' : 'wb-dialog-primary'" :disabled="busy || confirmDisabled" @click="emit('confirm')">
          {{ confirmLabel }}
        </button>
      </footer>
    </div>
  </div>
</template>
