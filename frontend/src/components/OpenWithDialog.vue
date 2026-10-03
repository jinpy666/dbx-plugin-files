<script setup lang="ts">
// 打开方式对话框（独立顶层弹窗）：为远程编辑（files/remote-edit/open）选择
// 启动目标——系统默认 / sidecar 编辑器目录条目（files/local/editors/list，
// parity with dbx-plugin-ssh local/editors/list）/ 本机探测预设 / 手输可执行
// 文件路径 / 自定义命令行（支持 {file} 占位符）。确认后 sidecar 把文件拉到
// 本机临时副本并按所选通道启动；本地保存由后台监视循环自动回传远端原路径
// （FinalShell 式远程编辑）。路径先经 files/validate-open-app、命令行先经
// 同一方法的 command 分支预校验（下载副本是昂贵前置，失败要早在启动前），
// 失败就地报错、不打断输入。
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { ExternalLink } from "@lucide/vue";

/** sidecar 编辑器目录条目（files/local/editors/list 的 editors[]）。 */
interface KnownEditor {
  id: string;
  name: string;
  available: boolean;
  launch: { kind: "openApp"; appId: string } | { kind: "exe"; exe: string; args: string[] };
  suggestedExtensions: string[];
}

/** 确认载荷：三选一通道（优先级 editorId > customCommand > app；空 app = 系统默认）。 */
export interface OpenWithConfirm {
  app: string;
  editorId?: string;
  customCommand?: string;
}

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  /** 本机探测到的应用预设（files/local/detect-apps，目录不可用时的兜底）。 */
  presets: Array<{ id: string; name: string; path: string }>;
  /** 按扩展名解析出的用户偏好应用（设置面板「打开方式」的当前默认）。 */
  initialApp?: string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "confirm", payload: OpenWithConfirm): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

/** sidecar 编辑器目录；空数组（旧 sidecar/拉取失败）时回落 detect-apps 预设。 */
const editors = ref<KnownEditor[]>([]);

const appDraft = ref(props.initialApp ?? "");
const commandDraft = ref("");
const selectedEditorId = ref("");
const validating = ref(false);
const error = ref("");

/** 可用编辑器 chips：目录可用走目录（含 TextEdit/Notepad/gedit 等文本编辑器
 * 与办公套件），不可用回落 detect-apps 路径预设（点选填路径草稿）。 */
const catalogEditors = computed(() => editors.value.filter((editor) => editor.available));
const useCatalog = computed(() => catalogEditors.value.length > 0);

function launchHint(editor: KnownEditor): string {
  return editor.launch.kind === "exe" ? editor.launch.exe : editor.launch.appId;
}

function selectEditor(editor: KnownEditor) {
  selectedEditorId.value = editor.id;
  appDraft.value = "";
  commandDraft.value = "";
  error.value = "";
}

function onAppInput(event: Event) {
  appDraft.value = (event.target as HTMLInputElement).value;
  selectedEditorId.value = "";
}

function onCommandInput(event: Event) {
  commandDraft.value = (event.target as HTMLInputElement).value;
  selectedEditorId.value = "";
}

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
  // 编辑器目录（parity with ssh local/editors/list）：失败静默回落预设，
  // 不打断「打开方式」主链路。
  try {
    const payload = await window.dbxPlugin.invoke<{ editors: KnownEditor[] }>(
      "files/local/editors/list",
    );
    editors.value = payload.editors ?? [];
  } catch {
    editors.value = [];
  }
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
  // 通道优先级：编辑器目录条目 > 自定义命令行 > 可执行文件路径。
  if (selectedEditorId.value) {
    emit("confirm", { app: "", editorId: selectedEditorId.value });
    return;
  }
  const command = commandDraft.value.trim();
  if (command) {
    validating.value = true;
    error.value = "";
    try {
      await window.dbxPlugin.invoke("files/local/validate-open-app", { command });
      emit("confirm", { app: "", customCommand: command });
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause);
    } finally {
      validating.value = false;
    }
    return;
  }
  const app = appDraft.value.trim();
  if (!app) {
    emit("confirm", { app: "" });
    return;
  }
  validating.value = true;
  error.value = "";
  try {
    await window.dbxPlugin.invoke("files/local/validate-open-app", { path: app });
    emit("confirm", { app });
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
        <div v-if="useCatalog" class="wb-presets">
          <span class="wb-muted">{{ t("presetsTitle") }}</span>
          <div class="wb-presets-row">
            <button
              v-for="editor in catalogEditors"
              :key="editor.id"
              type="button"
              class="wb-preset-chip"
              :class="{ 'wb-preset-chip-active': selectedEditorId === editor.id }"
              :title="launchHint(editor)"
              :aria-pressed="selectedEditorId === editor.id"
              @click="selectEditor(editor)"
            >{{ editor.name }}</button>
          </div>
        </div>
        <div v-else-if="presets.length" class="wb-presets">
          <span class="wb-muted">{{ t("presetsTitle") }}</span>
          <div class="wb-presets-row">
            <button
              v-for="preset in presets"
              :key="preset.id"
              type="button"
              class="wb-preset-chip"
              :title="preset.path"
              @click="appDraft = preset.path; selectedEditorId = ''; commandDraft = ''"
            >{{ preset.name }}</button>
          </div>
        </div>
        <label>
          <span class="wb-muted">{{ t("openWithCustomLabel") }}</span>
          <input
            :value="appDraft"
            class="wb-mono"
            spellcheck="false"
            :placeholder="t('openWithCustomPlaceholder')"
            :aria-invalid="Boolean(error)"
            @input="onAppInput"
            @keydown.enter="confirm"
          />
        </label>
        <label>
          <span class="wb-muted">{{ t("openWithCommandLabel") }}</span>
          <input
            :value="commandDraft"
            class="wb-mono"
            spellcheck="false"
            :placeholder="t('openWithCommandPlaceholder')"
            :aria-invalid="Boolean(error)"
            @input="onCommandInput"
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
