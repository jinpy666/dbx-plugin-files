<script setup lang="ts">
import { ref } from "vue";
import { Columns2, Download, FolderPlus, Gauge, ScrollText, Trash2, Upload, Plug } from "@lucide/vue";

// 全局动作栏：路径/面包屑/过滤等栏内控件已下沉到各栏 wb-pane-header
// （双栏对称性修复），这里只承载跨栏的全局操作。
const props = defineProps<{
  canWrite: boolean;
  busy: boolean;
  hasSelection: boolean;
  dockOpen: boolean;
  dockTab: "transfers" | "audit" | "connection";
  dualPane: boolean;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "new-folder"): void;
  /** files 为 null 表示走宿主 fileTransfer picker。 */
  (event: "upload", files: File[] | null): void;
  (event: "download"): void;
  (event: "delete"): void;
  (event: "toggle-dock", tab: "transfers" | "audit" | "connection"): void;
  (event: "toggle-dual-pane"): void;
}>();

function t(key: string, values?: Record<string, string | number>) {
  return props.t(key, values);
}

const fileInput = ref<HTMLInputElement>();

function pickFiles() {
  if (window.dbxPlugin.fileTransfer) {
    emit("upload", null);
    return;
  }
  fileInput.value?.click();
}

function onPicked(event: Event) {
  const input = event.target as HTMLInputElement;
  const files = Array.from(input.files ?? []);
  if (files.length) emit("upload", files);
  input.value = "";
}
</script>

<template>
  <header class="wb-toolbar">
    <div class="wb-toolbar-actions">
      <button class="wb-toolbar-button" :title="t('newFolder')" :disabled="!canWrite || busy" @click="emit('new-folder')"><FolderPlus /> {{ t("newFolder") }}</button>
      <button class="wb-toolbar-button" :title="t('upload')" :disabled="!canWrite || busy" @click="pickFiles"><Upload /> {{ t("upload") }}</button>
      <input ref="fileInput" type="file" multiple class="hidden" @change="onPicked" />
      <button class="wb-toolbar-button" :title="t('download')" :disabled="!hasSelection || busy" @click="emit('download')"><Download /> {{ t("download") }}</button>
      <button class="wb-toolbar-button" :title="t('deleteSelected')" :disabled="!hasSelection || !canWrite || busy" @click="emit('delete')"><Trash2 /> {{ t("deleteSelected") }}</button>
      <span class="wb-toolbar-separator" aria-hidden="true" />
      <button class="wb-icon-button wb-icon-neutral" :title="t('dualPane')" :class="{ 'is-active': dualPane }" @click="emit('toggle-dual-pane')"><Columns2 /></button>
      <button class="wb-icon-button wb-icon-neutral" :title="t('transfers')" :class="{ 'is-active': dockOpen && dockTab === 'transfers' }" @click="emit('toggle-dock', 'transfers')"><Gauge /></button>
      <button class="wb-icon-button wb-icon-neutral" :title="t('auditPanel')" :class="{ 'is-active': dockOpen && dockTab === 'audit' }" @click="emit('toggle-dock', 'audit')"><ScrollText /></button>
      <button class="wb-icon-button wb-icon-neutral" :title="t('connectionPanel')" :class="{ 'is-active': dockOpen && dockTab === 'connection' }" @click="emit('toggle-dock', 'connection')"><Plug /></button>
    </div>
  </header>
</template>
