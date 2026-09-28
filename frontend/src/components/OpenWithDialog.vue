<script setup lang="ts">
// 打开方式对话框（独立顶层弹窗）：为远程编辑（files/remote-edit/open）选择
// 启动应用——系统默认 / 本机探测预设 / 手输可执行文件路径。确认后 sidecar
// 把文件拉到本机临时副本并启动所选应用；本地保存由后台监视循环自动回传
// 远端原路径（FinalShell 式远程编辑）。非空路径先经 files/validate-open-app
// 同一套校验（存在 + 单一可执行文件），失败就地报错、不打断输入。
import { nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { ExternalLink } from "@lucide/vue";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  /** 本机探测到的应用预设（files/local/detect-apps，工作台启动时已拉取）。 */
  presets: Array<{ id: string; name: string; path: string }>;
  /** 按扩展名解析出的用户偏好应用（设置面板「打开方式」的当前默认）。 */
  initialApp?: string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  /** app 空串 = 系统默认应用。 */
  (event: "confirm", app: string): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

const appDraft = ref(props.initialApp ?? "");
const validating = ref(false);
const error = ref("");

// 焦点管理与 ConfirmDialog 同款（父层 v-if 挂载，故走生命周期钩子）：
// 打开即聚焦输入框；Tab 循环困在弹层内；关闭归还触发元素焦点。
const dialogEl = ref<HTMLElement>();
let returnFocusTo: HTMLElement | null = null;
const FOCUSABLE_SELECTOR = "button:not([disabled]), input:not([disabled]), a[href]";

onMounted(async () => {
  returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  await nextTick();
  const input = dialogEl.value?.querySelector<HTMLInputElement>("input");
  input?.focus();
  input?.select();
});

onBeforeUnmount(() => {
  returnFocusTo?.focus();
  returnFocusTo = null;
});

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

async function confirm() {
  if (validating.value) return;
  const app = appDraft.value.trim();
  if (!app) {
    emit("confirm", "");
    return;
  }
  validating.value = true;
  error.value = "";
  try {
    await window.dbxPlugin.invoke("files/local/validate-open-app", { path: app });
    emit("confirm", app);
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause);
  } finally {
    validating.value = false;
  }
}
</script>

<template>
  <div class="wb-dialog-backdrop" @click.self="emit('close')">
    <div ref="dialogEl" class="wb-dialog" role="dialog" aria-modal="true" :aria-label="t('openWithTitle')" @keydown="onTabKeydown">
      <header>{{ t("openWithTitle") }}</header>
      <div class="wb-dialog-body">
        <p style="margin: 0 0 6px">{{ t("openWithHint") }}</p>
        <div v-if="presets.length" class="wb-presets">
          <span class="wb-muted">{{ t("presetsTitle") }}</span>
          <div class="wb-presets-row">
            <button
              v-for="preset in presets"
              :key="preset.id"
              type="button"
              class="wb-preset-chip"
              :title="preset.path"
              @click="appDraft = preset.path"
            >{{ preset.name }}</button>
          </div>
        </div>
        <label>
          <span class="wb-muted">{{ t("openWithCustomLabel") }}</span>
          <input
            v-model="appDraft"
            class="wb-mono"
            spellcheck="false"
            :placeholder="t('openWithCustomPlaceholder')"
            :aria-invalid="Boolean(error)"
            @keydown.enter="confirm"
          />
        </label>
        <p v-if="error" class="wb-dialog-warning" role="alert">{{ error }}</p>
      </div>
      <footer>
        <button type="button" class="wb-dialog-cancel" :disabled="validating" @click="emit('close')">{{ t("cancel") }}</button>
        <button type="button" class="wb-dialog-primary" :disabled="validating" @click="confirm">{{ t("openWithConfirm") }}</button>
      </footer>
    </div>
  </div>
</template>
