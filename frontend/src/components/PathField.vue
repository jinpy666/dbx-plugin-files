<script setup lang="ts">
// 路径栏（tiny-rdm 对标）：单一控件替代「面包屑 + 输入框」并排的重复形态——
// 默认展示可点击面包屑，点左侧文件夹图标进入编辑态（input 全选），
// Enter 跳转、Esc 还原、失焦时有改动则提交。
import { nextTick, ref, watch } from "vue";
import { Folder } from "@lucide/vue";
import Breadcrumbs from "./Breadcrumbs.vue";

const props = defineProps<{
  path: string;
  maxVisible?: number;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "navigate", path: string): void;
}>();

const editing = ref(false);
const draft = ref(props.path);
const inputEl = ref<HTMLInputElement>();

// 外部跳转（快速目录/表格/工具栏）后退出编辑态并同步草稿。
watch(
  () => props.path,
  (next) => {
    draft.value = next;
    editing.value = false;
  },
);

async function startEdit() {
  draft.value = props.path;
  editing.value = true;
  await nextTick();
  inputEl.value?.focus();
  inputEl.value?.select();
}

function submit() {
  const next = draft.value.trim();
  editing.value = false;
  if (next && next !== props.path) emit("navigate", next);
  else draft.value = props.path;
}

function cancel() {
  editing.value = false;
  draft.value = props.path;
}

function onBlur() {
  if (draft.value.trim() && draft.value.trim() !== props.path) submit();
  else cancel();
}
</script>

<template>
  <div class="wb-path-field">
    <button type="button" class="wb-icon-button wb-path-edit" :title="t('editPath')" @click="startEdit">
      <Folder aria-hidden="true" />
    </button>
    <Breadcrumbs v-if="!editing" :path="path" :max-visible="maxVisible ?? 3" @navigate="emit('navigate', $event)" />
    <input
      v-else
      ref="inputEl"
      v-model="draft"
      class="wb-path-input"
      :placeholder="t('pathPlaceholder')"
      spellcheck="false"
      @keydown.enter.prevent="submit"
      @keydown.esc.prevent="cancel"
      @blur="onBlur"
    />
  </div>
</template>
