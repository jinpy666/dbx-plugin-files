<script setup lang="ts">
// 路径目标输入行（ConfirmDialog 系右键表单共用）：输入框 + 内嵌目录选择器
// 开关，交互与 SyncDialog 的路径行一致——激活按钮高亮、进入子目录即回填
// 输入框；浏览器从当前值父目录起步（父目录不存在时给出中性提示）。
// mode="file"（另存为式）：目标是文件路径，进入目录后保留原文件名拼接到
// 目录下（如 /docs.tar.gz → /media/docs.tar.gz），避免把目录本身填成目标。
import { ref } from "vue";
import { FolderOpen } from "@lucide/vue";
import { baseName, joinPath, parentPath } from "../lib/api";
import DirectoryBrowser from "./DirectoryBrowser.vue";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  modelValue: string;
  label: string;
  placeholder?: string;
  /** 浏览目标连接（源/目标同连接，由父层固化）。 */
  connectionId: string;
  /** dir（默认）= 进入目录即选中该目录；file = 保留原文件名拼进目录。 */
  mode?: "dir" | "file";
}>();

const emit = defineEmits<{
  (event: "update:modelValue", value: string): void;
  /** 输入框内 Enter：交父层走确认链路（父层做危险/可用性判断）。 */
  (event: "submit"): void;
}>();

const browsing = ref(false);
const browserStart = ref("");

function toggleBrowse() {
  if (browsing.value) {
    browsing.value = false;
    return;
  }
  // 从当前值父目录起步（根/空值落回根）。
  const current = props.modelValue.trim();
  browserStart.value = current ? parentPath(current) : "/";
  browsing.value = true;
}

function onNavigate(path: string) {
  if (props.mode === "file") {
    const base = baseName(props.modelValue.trim());
    emit("update:modelValue", base ? joinPath(path, base) : path);
    return;
  }
  emit("update:modelValue", path);
}

function onInput(event: Event) {
  emit("update:modelValue", (event.target as HTMLInputElement).value);
}
</script>

<template>
  <div class="wb-confirm-field">
    <span>{{ label }}</span>
    <div class="wb-sync-path-row">
      <input
        :value="modelValue"
        class="wb-mono"
        spellcheck="false"
        :placeholder="placeholder"
        :aria-label="label"
        @input="onInput"
        @keydown.enter.prevent="emit('submit')"
      />
      <button
        type="button"
        class="wb-icon-button wb-icon-neutral wb-sync-browse"
        :class="{ 'wb-sync-browse-active': browsing }"
        :aria-label="t('syncBrowsePath')"
        v-tip="t('syncBrowsePath')"
        @click="toggleBrowse"
      ><FolderOpen /></button>
    </div>
    <div v-if="browsing" class="wb-confirm-browser">
      <DirectoryBrowser
        :key="browserStart"
        :t="t"
        :connection-id="connectionId"
        :initial-path="browserStart"
        missing-hint-key="syncBrowseMissing"
        @navigate="onNavigate"
      />
    </div>
  </div>
</template>
