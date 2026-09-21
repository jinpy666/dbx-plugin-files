<script setup lang="ts">
// 仅桌面功能卡片：Web 端打开设置时，对本机才能执行的功能给出友好的能力说明，
// 替代"直接隐藏"或"一行为空"的两种旧做法。
import { Monitor } from "@lucide/vue";

defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();
</script>

<template>
  <div class="wb-desktop-only-card" role="note">
    <Monitor class="wb-doc-card-icon" aria-hidden="true" />
    <div class="wb-doc-card-body">
      <strong>{{ t("desktopOnlyTitle") }}</strong>
      <p>{{ t("desktopOnlyDesc") }}</p>
    </div>
  </div>
</template>
