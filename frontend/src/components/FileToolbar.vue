<script setup lang="ts">
import { computed, ref } from "vue";
import { Columns2, Download, FolderPlus, Gauge, HardDrive, ScrollText, Settings, Trash2, Upload, Plug } from "@lucide/vue";

// 全局动作栏：路径/面包屑/过滤等栏内控件已下沉到各栏 wb-pane-header
// （双栏对称性修复），这里只承载跨栏的全局操作。
const props = defineProps<{
  canWrite: boolean;
  busy: boolean;
  hasSelection: boolean;
  dockOpen: boolean;
  dockTab: "transfers" | "audit" | "connection";
  dualPane: boolean;
  /** 顶栏 identity（对标 ssh 工具栏左侧）：连接名/色条/只读徽章 + 状态 pill。 */
  connectionName: string;
  connectionColor?: string;
  readOnly: boolean;
  connState: "connecting" | "connected" | "disconnected";
  /** 挂载是桌面能力（web/docker 无本机可挂）：false 时整组入口隐藏。 */
  showMount: boolean;
  /** 挂载入口只对远端连接面可用（本地 __local__ 栏没有可挂载的远端）。 */
  canMount: boolean;
  /** 当前生效的传输限速（files/bwlimit；null/空 = 不限速）：非空时顶栏显示徽标。 */
  bwlimit?: string | null;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "new-folder"): void;
  /** files 为 null 表示走宿主 fileTransfer picker。 */
  (event: "upload", files: File[] | null): void;
  (event: "download"): void;
  (event: "delete"): void;
  /** 审计#17：工具栏只保留一个 dock 开关。tab 省略即切换「当前页签」的
   * 开/关；带 tab 时语义不变（打开指定页签）——App 层兼容两种调用。 */
  (event: "toggle-dock", tab?: "transfers" | "audit" | "connection"): void;
  (event: "toggle-dual-pane"): void;
  /** 本地挂载（对标 ssh 工具栏动作 icon）：挂载活动栏当前目录。 */
  (event: "mount"): void;
  /** 独立设置弹窗（对标 ssh 设置 icon）：设置不再挤在 dock 页签里。 */
  (event: "open-settings"): void;
  (event: "bwlimit-click"): void;
}>();

function t(key: string, values?: Record<string, string | number>) {
  return props.t(key, values);
}

// 审计#17：4 个 per-dock 图标收敛为单个开关按钮——按钮即「当前 dock 页签」
// （图标与提示随 dockTab 走，is-active 表示 dock 已打开），开/关切换在
// 工具栏，页签切换只保留 dock 内 tablist 一处。settings 已拆为独立弹窗。
const DOCK_ICONS = {
  transfers: Gauge,
  audit: ScrollText,
  connection: Plug,
} as const;

const DOCK_TIP_KEYS: Record<keyof typeof DOCK_ICONS, string> = {
  transfers: "transfers",
  audit: "auditPanel",
  connection: "connectionPanel",
};

const dockIcon = computed(() => DOCK_ICONS[props.dockTab]);
const dockTipKey = computed(() => DOCK_TIP_KEYS[props.dockTab]);

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
  <header class="wb-toolbar" @contextmenu.prevent>
    <div class="wb-identity">
      <span v-if="connectionColor" class="wb-connection-color" :style="{ background: connectionColor }" />
      <strong :title="connectionName">{{ connectionName }}</strong>
      <span v-if="readOnly" class="wb-readonly-badge">{{ t("readOnly") }}</span>
      <span class="wb-session-pill" :class="`session-${connState}`"><span class="wb-session-dot" aria-hidden="true" />{{ t(`sessionStatus.${connState}`) }}</span>
    <!-- 限速生效徽标：点击直达设置传输页签；Gauge 图标语义=速率。 -->
    <button v-if="bwlimit" type="button" class="wb-session-pill wb-bwlimit-pill" v-tip="t('bwlimitBadgeTip', { rate: bwlimit })" @click="emit('bwlimit-click')"><Gauge /> {{ t("bwlimitBadge") }} {{ bwlimit }}</button>
    </div>
    <div class="wb-toolbar-actions">
      <!-- 审计中#13：文字按钮统一 v-tip（宿主 webview 不渲染原生 title）。 -->
      <button class="wb-toolbar-button" v-tip="t('newFolder')" :disabled="!canWrite || busy" @click="emit('new-folder')"><FolderPlus /> {{ t("newFolder") }}</button>
      <button class="wb-toolbar-button" v-tip="t('upload')" :disabled="!canWrite || busy" @click="pickFiles"><Upload /> {{ t("upload") }}</button>
      <input ref="fileInput" type="file" multiple class="hidden" @change="onPicked" />
      <button class="wb-toolbar-button" v-tip="t('download')" :disabled="!hasSelection || busy" @click="emit('download')"><Download /> {{ t("download") }}</button>
      <button class="wb-toolbar-button" v-tip="t('deleteSelected')" :disabled="!hasSelection || !canWrite || busy" @click="emit('delete')"><Trash2 /> {{ t("deleteSelected") }}</button>
      <span class="wb-toolbar-separator" aria-hidden="true" />
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('dualPane')" :class="{ 'is-active': dualPane }" @click="emit('toggle-dual-pane')"><Columns2 /></button>
      <!-- 审计#17：单一 dock 开关（高亮=已打开；图标/提示=当前页签）。 -->
      <button class="wb-icon-button wb-icon-neutral" v-tip="t(dockTipKey)" :class="{ 'is-active': dockOpen }" :aria-pressed="dockOpen" @click="emit('toggle-dock')"><component :is="dockIcon" /></button>
      <!-- 功能 icon（对标 ssh 工具栏）：挂载活动栏目录 + 打开独立设置弹窗。 -->
      <button v-if="showMount" class="wb-icon-button wb-icon-emerald" v-tip="t('mountToLocal')" :disabled="!canMount" @click="emit('mount')"><HardDrive /></button>
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('settings')" @click="emit('open-settings')"><Settings /></button>
    </div>
  </header>
</template>
