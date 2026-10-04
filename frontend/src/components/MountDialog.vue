<script setup lang="ts">
// 挂载到本机对话框（独立顶层弹窗，z-index 高于设置/预览层）：内嵌本机目录
// 浏览器（DirectoryBrowser 组件，与设置下载目录共用）选目录后确认挂载——不依
// 赖宿主 pickDirectory。挂载位置留空 = sidecar 默认（~/dbx-files-mounts/<连接>）；
// 所选目录不存在时由后端挂载时自动创建。
import { nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { HardDrive, X } from "@lucide/vue";
import DirectoryBrowser from "./DirectoryBrowser.vue";
import { trapTabKey } from "../lib/a11y";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  /** mountPoint 为空串 = 使用 sidecar 默认位置。 */
  (event: "confirm", mountPoint: string): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

/** 输入框（编辑期真源）：空 = 默认位置；浏览器导航时同步回填。 */
const inputPath = ref("");
const browserRef = ref<{ browse: (path: string) => Promise<void> } | null>(null);

function onBrowserNavigate(path: string) {
  inputPath.value = path;
}

/** 使用默认位置：清空输入并把浏览器带回 home。 */
function useDefault() {
  inputPath.value = "";
  void browserRef.value?.browse("/");
}

// 焦点约定（SyncDialog/ScheduleDialog 同款，0.1.89 review）：此前仅声明
// aria-modal 却没有任何焦点管理——Tab 可以走出被遮罩挡住的背景，打开时
// 焦点不进入弹层（屏幕阅读器不会播报），关闭后焦点不归还。打开聚焦挂载
// 点输入框；Tab 在弹层内循环；卸载后焦点归还触发元素。
const dialogEl = ref<HTMLElement | null>(null);
let returnFocusTo: HTMLElement | null = null;

onMounted(async () => {
  returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  await nextTick();
  dialogEl.value?.querySelector<HTMLInputElement>("input")?.focus();
});

onBeforeUnmount(() => {
  returnFocusTo?.focus();
  returnFocusTo = null;
});

function onTabKeydown(event: KeyboardEvent): void {
  if (event.key !== "Tab") return;
  trapTabKey(event, dialogEl.value);
}
</script>

<template>
  <div class="wb-mount-backdrop" role="dialog" aria-modal="true" :aria-label="t('mountToLocal')" @click.self="emit('close')">
    <div ref="dialogEl" class="wb-mount-dialog" @keydown="onTabKeydown">
      <header>
        <strong><HardDrive class="wb-mount-title-icon" /> {{ t("mountToLocal") }}</strong>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="emit('close')"><X /></button>
      </header>
      <p class="wb-mount-hint">{{ t("mountDialogBody") }}</p>
      <!-- 挂载位置：手输 + 内嵌本机目录浏览（面包屑 / 上级 / 子目录）。 -->
      <label class="wb-mount-field">
        <span>{{ t("mountPointLabel") }}</span>
        <input
          v-model="inputPath"
          class="wb-mono"
          spellcheck="false"
          :placeholder="t('mountPointPlaceholder')"
          @keydown.enter.prevent="browserRef?.browse(inputPath)"
        />
      </label>
      <div class="wb-mount-browse-toolbar">
        <button class="wb-link-button" type="button" @click="useDefault">{{ t("mountUseDefault") }}</button>
      </div>
      <DirectoryBrowser ref="browserRef" :t="t" @navigate="onBrowserNavigate" />
      <footer>
        <span class="wb-muted wb-mount-foot-hint">{{ t("mountPointHint") }}</span>
        <span class="wb-mount-foot-actions">
          <button class="wb-dialog-cancel" type="button" @click="emit('close')">{{ t("cancel") }}</button>
          <button class="wb-dialog-primary" type="button" @click="emit('confirm', inputPath.trim())">{{ t("mountConfirm") }}</button>
        </span>
      </footer>
    </div>
  </div>
</template>
