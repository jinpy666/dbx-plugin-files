<script setup lang="ts">
// 可点击面包屑（A-FILES ④a）：逐级导航 + 中间层级折叠（点击省略号展开）。
import { computed, ref, watch } from "vue";
import { collapseCrumbs, parseCrumbs } from "../lib/breadcrumbs";
import { decodeDisplayName } from "../lib/charset";

const props = defineProps<{
  path: string;
  /** 超过该级数时折叠中间层级。 */
  maxVisible?: number;
  /** FTP 显示字符集（issue #32）；空 = 不做显示解码。 */
  displayCharset?: string;
}>();

const emit = defineEmits<{
  (event: "navigate", path: string): void;
}>();

const expanded = ref(false);
watch(
  () => props.path,
  () => (expanded.value = false),
);

const items = computed(() => {
  const charset = props.displayCharset ?? "";
  const crumbs = parseCrumbs(props.path).map((item) =>
    "collapsed" in item || !charset ? item : { ...item, name: decodeDisplayName(item.name, charset) },
  );
  return expanded.value ? crumbs : collapseCrumbs(crumbs, props.maxVisible ?? 4);
});
</script>

<template>
  <nav class="wb-breadcrumbs">
    <template v-for="(item, index) in items" :key="'collapsed' in item ? 'ellipsis' : item.path">
      <!-- 首段恒为根 crumb（自身渲染 "/"），其后第一段不再补分隔符，
           否则视觉成 `//Users/demo`（P1-1）；折叠态同理跳过根后的省略号。 -->
      <span v-if="index > 1" class="wb-crumb-sep">/</span>
      <button
        v-if="'collapsed' in item"
        type="button"
        class="wb-crumb-ellipsis"
        v-tip="path"
        @click.stop="expanded = true"
      >…</button>
      <button
        v-else
        type="button"
        :class="{ 'is-current': item.path === path }"
        @click="emit('navigate', item.path)"
      >{{ item.name }}</button>
    </template>
  </nav>
</template>
