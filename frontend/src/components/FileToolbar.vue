<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { Activity, CircleGauge, Columns2, Download, FolderPlus, Gauge, HardDrive, ScrollText, Settings, Star, Trash2, Upload, Plug } from "@lucide/vue";

// 全局动作栏：路径/面包屑/过滤等栏内控件已下沉到各栏 wb-pane-header
// （双栏对称性修复），这里只承载跨栏的全局操作。
const props = defineProps<{
  canWrite: boolean;
  busy: boolean;
  hasSelection: boolean;
  dockOpen: boolean;
  dockTab: "transfers" | "audit" | "connection" | "stats";
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
  /** 活动栏当前目录是否已收藏（rclone-ui parity 星标）：点亮时图标高亮。 */
  starred: boolean;
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
  (event: "toggle-dock", tab?: "transfers" | "audit" | "connection" | "stats"): void;
  (event: "toggle-dual-pane"): void;
  /** 本地挂载（对标 ssh 工具栏动作 icon）：挂载活动栏当前目录。 */
  (event: "mount"): void;
  /** 独立设置弹窗（对标 ssh 设置 icon）：设置不再挤在 dock 页签里。 */
  (event: "open-settings"): void;
  (event: "bwlimit-click"): void;
  /** 限速快捷菜单直接落值（"" = off）：App 复用设置页保存链路（含通知）。 */
  (event: "bwlimit-set", rate: string): void;
  /** 收藏切换（rclone-ui parity）：作用于活动栏当前目录，App 负责落 prefs。 */
  (event: "toggle-favorite"): void;
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
  stats: Activity,
} as const;

const DOCK_TIP_KEYS: Record<keyof typeof DOCK_ICONS, string> = {
  transfers: "transfers",
  audit: "auditPanel",
  connection: "connectionPanel",
  stats: "statsPanel",
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

/** 供 App 在宿主文件桥读盘失败时回退原生选择器（File API，不依赖宿主句柄）。 */
function openNativePicker() {
  fileInput.value?.click();
}

defineExpose({ openNativePicker });

// ---- 限速（rclone 令牌桶）快捷开关：常驻工具栏 icon + 下拉菜单 -------------
// 旧实现只在限速生效时显示徽标 pill（未设限速时入口不可见，用户找不到切换
// 的地方）；现在常驻 CircleGauge 按钮，菜单里可一键设预设/关闭/去设置自定义。
// 预设值为 rclone 可接受的「数字+单位」（M = MiB/s，与设置页单位语义一致）。
const BWLIMIT_PRESETS: ReadonlyArray<{ label: string; value: string }> = [
  { label: "1 MB/s", value: "1M" },
  { label: "5 MB/s", value: "5M" },
  { label: "10 MB/s", value: "10M" },
  { label: "50 MB/s", value: "50M" },
];

const bwlimitOpen = ref(false);
const bwlimitWrap = ref<HTMLElement>();

const bwlimitTip = computed(() =>
  props.bwlimit ? props.t("bwlimitBadgeTip", { rate: props.bwlimit }) : props.t("bwlimitLabel"),
);

function toggleBwlimitMenu() {
  bwlimitOpen.value = !bwlimitOpen.value;
}

function closeBwlimitMenu() {
  bwlimitOpen.value = false;
}

function applyBwlimit(rate: string) {
  closeBwlimitMenu();
  emit("bwlimit-set", rate);
}

function openBwlimitSettings() {
  closeBwlimitMenu();
  emit("bwlimit-click");
}

// 点击菜单外关闭（捕获阶段，避免菜单项 click 前菜单已被卸载）。
function onDocumentPointerdown(event: PointerEvent) {
  if (bwlimitOpen.value && bwlimitWrap.value && !bwlimitWrap.value.contains(event.target as Node)) {
    closeBwlimitMenu();
  }
}

onMounted(() => document.addEventListener("pointerdown", onDocumentPointerdown, true));
onBeforeUnmount(() => document.removeEventListener("pointerdown", onDocumentPointerdown, true));

/** 统计 icon 与单 dock 开关在 stats 页签打开时同图标：此时隐藏独立入口，
 * 避免相邻两个 Activity 按钮造成困惑（dock 开关本身已高亮表示当前页签）。 */
const statsDockOpen = computed(() => props.dockOpen && props.dockTab === "stats");
</script>

<template>
  <header class="wb-toolbar" @contextmenu.prevent>
    <div class="wb-identity">
      <span v-if="connectionColor" class="wb-connection-color" :style="{ background: connectionColor }" />
      <strong :title="connectionName">{{ connectionName }}</strong>
      <span v-if="readOnly" class="wb-readonly-badge">{{ t("readOnly") }}</span>
      <span class="wb-session-pill" :class="`session-${connState}`"><span class="wb-session-dot" aria-hidden="true" />{{ t(`sessionStatus.${connState}`) }}</span>
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
      <!-- 收藏星标（rclone-ui parity）：收藏/取消收藏活动栏当前目录；is-active=已收藏。 -->
      <button
        class="wb-icon-button wb-icon-neutral wb-fav-toggle"
        v-tip="t(starred ? 'favRemove' : 'favAdd')"
        :class="{ 'is-active': starred }"
        :aria-pressed="starred"
        @click="emit('toggle-favorite')"
      ><Star /></button>
      <!-- 限速（rclone 令牌桶）常驻开关：is-active=限速中，按钮随限速值内联显示
           当前速率；点击弹快捷菜单（预设/关闭/自定义），替代旧「仅生效时可见」的徽标。 -->
      <span ref="bwlimitWrap" class="wb-bwlimit-wrap">
        <button
          class="wb-icon-button wb-bwlimit-toggle"
          v-tip="bwlimitTip"
          :class="{ 'is-active': Boolean(bwlimit) }"
          :aria-expanded="bwlimitOpen"
          aria-haspopup="menu"
          @click="toggleBwlimitMenu"
        ><CircleGauge /><span v-if="bwlimit" class="wb-bwlimit-rate">{{ bwlimit }}</span></button>
        <div v-if="bwlimitOpen" class="wb-bwlimit-menu" role="menu" @keydown.esc.stop="closeBwlimitMenu">
          <div class="wb-bwlimit-menu-title">{{ t("bwlimitLabel") }}</div>
          <div class="wb-bwlimit-menu-current">{{ bwlimit ?? t("bwlimitUnlimited") }}</div>
          <button v-for="preset in BWLIMIT_PRESETS" :key="preset.value" role="menuitem" type="button" :class="{ 'is-current': bwlimit === preset.value }" @click="applyBwlimit(preset.value)">{{ preset.label }}</button>
          <button role="menuitem" type="button" :class="{ 'is-current': !bwlimit }" @click="applyBwlimit('')">{{ t("bwlimitUnlimited") }}</button>
          <button role="menuitem" type="button" @click="openBwlimitSettings">{{ t("bwlimitCustom") }}</button>
        </div>
      </span>
      <!-- 统计页签（对标 rclone-dashboard overview：吞吐曲线 + 任务/空间汇总）：
           stats 页签已打开时由上方 dock 开关承载，独立入口隐藏。 -->
      <button v-if="!statsDockOpen" class="wb-icon-button wb-icon-neutral wb-stats-toggle" v-tip="t('statsPanel')" :class="{ 'is-active': statsDockOpen }" :aria-pressed="statsDockOpen" @click="emit('toggle-dock', 'stats')"><Activity /></button>
      <!-- 审计#17：单一 dock 开关（高亮=已打开；图标/提示=当前页签）。 -->
      <button class="wb-icon-button wb-icon-neutral" v-tip="t(dockTipKey)" :class="{ 'is-active': dockOpen }" :aria-pressed="dockOpen" @click="emit('toggle-dock')"><component :is="dockIcon" /></button>
      <!-- 功能 icon（对标 ssh 工具栏）：挂载活动栏目录 + 打开独立设置弹窗。 -->
      <button v-if="showMount" class="wb-icon-button wb-icon-emerald" v-tip="t('mountToLocal')" :disabled="!canMount" @click="emit('mount')"><HardDrive /></button>
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('settings')" @click="emit('open-settings')"><Settings /></button>
    </div>
  </header>
</template>
