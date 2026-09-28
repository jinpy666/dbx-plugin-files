<script setup lang="ts">
// 连接选择下拉（双栏 pane 顶条）：原生 select 的弹层无法跟随宿主主题与
// 工作台菜单视觉，这里换成与 .wb-context-menu 同规格的自定义下拉——
// 触发钮显示当前连接名（省略号 + 悬停全名），弹层列表带当前项勾选、
// 键盘导航（↑↓ 移动 / Enter 选择 / Esc 关闭）、点击外部关闭。
// 连接切换语义（含失败回退）由父层 change 处理，组件只管选择交互。
import { computed, nextTick, onBeforeUnmount, ref, watch } from "vue";
import { Check, ChevronDown } from "@lucide/vue";

const props = defineProps<{
  modelValue: string;
  options: Array<{ id: string; name: string }>;
  /** 无障碍标签（触发钮与弹层共用）；以 label 传入避免 aria-* 前缀被 Vue 当作纯属性。 */
  label: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "change", value: string): void;
}>();

const open = ref(false);
const root = ref<HTMLElement>();
const trigger = ref<HTMLButtonElement>();
const menu = ref<HTMLElement>();

const currentLabel = computed(() => props.options.find((option) => option.id === props.modelValue)?.name ?? props.t("sameConnection"));
const activeIndex = ref(0);

function optionButtons(): HTMLButtonElement[] {
  return Array.from(menu.value?.querySelectorAll<HTMLButtonElement>("[role='option']") ?? []);
}

function syncActiveIndex() {
  const index = props.options.findIndex((option) => option.id === props.modelValue);
  activeIndex.value = index >= 0 ? index : 0;
}

async function openMenu() {
  if (open.value) return close();
  syncActiveIndex();
  open.value = true;
  document.addEventListener("mousedown", onOutside, true);
  await nextTick();
  // 弹层定位在触发钮正下、右对齐（pane 顶条右对齐布局），贴视口右缘时左移。
  const anchor = trigger.value?.getBoundingClientRect();
  const list = menu.value;
  if (anchor && list) {
    const width = Math.max(anchor.width, 176);
    const left = Math.max(4, Math.min(anchor.right - width, window.innerWidth - width - 4));
    const top = anchor.bottom + 4;
    list.style.left = `${Math.round(left)}px`;
    list.style.top = `${Math.round(top)}px`;
    list.style.width = `${Math.round(width)}px`;
  }
  optionButtons()[activeIndex.value]?.focus();
}

function close(refocus = false) {
  if (!open.value) return;
  open.value = false;
  document.removeEventListener("mousedown", onOutside, true);
  if (refocus) trigger.value?.focus();
}

function onOutside(event: MouseEvent) {
  if (!root.value?.contains(event.target as Node)) close();
}

function choose(id: string) {
  close(true);
  if (id !== props.modelValue) emit("change", id);
}

function onOptionKeydown(event: KeyboardEvent, index: number) {
  const buttons = optionButtons();
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const delta = event.key === "ArrowDown" ? 1 : -1;
    const next = (index + delta + buttons.length) % buttons.length;
    activeIndex.value = next;
    buttons[next]?.focus();
  } else if (event.key === "Home") {
    event.preventDefault();
    activeIndex.value = 0;
    buttons[0]?.focus();
  } else if (event.key === "End") {
    event.preventDefault();
    activeIndex.value = buttons.length - 1;
    buttons[buttons.length - 1]?.focus();
  } else if (event.key === "Escape") {
    event.preventDefault();
    close(true);
  }
}

watch(
  () => props.modelValue,
  () => syncActiveIndex(),
);

onBeforeUnmount(() => document.removeEventListener("mousedown", onOutside, true));
</script>

<template>
  <div ref="root" class="wb-conn-select">
    <button
      ref="trigger"
      type="button"
      class="wb-conn-select-trigger"
      :aria-label="label"
      aria-haspopup="listbox"
      :aria-expanded="open"
      :title="currentLabel"
      @click="openMenu"
      @keydown.down.prevent="openMenu"
      @keydown.up.prevent="openMenu"
    >
      <span class="wb-conn-select-label">{{ currentLabel }}</span>
      <ChevronDown class="wb-conn-select-caret" :size="12" />
    </button>
    <div v-if="open" ref="menu" class="wb-conn-select-menu" role="listbox" :aria-label="label">
      <button
        v-for="(option, index) in options"
        :key="option.id"
        type="button"
        role="option"
        :aria-selected="option.id === modelValue"
        :class="{ 'is-active': option.id === modelValue }"
        @click="choose(option.id)"
        @keydown="onOptionKeydown($event, index)"
        @mousemove="activeIndex = index"
      >
        <span class="wb-conn-select-name">{{ option.name }}</span>
        <Check v-if="option.id === modelValue" :size="14" />
      </button>
    </div>
  </div>
</template>
