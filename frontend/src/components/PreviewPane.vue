<script setup lang="ts">
// 预览两套方案：文本/代码走 CodeMirror，浏览器原生图片走 <img>，归档走 files/archiveList，
// 未知扩展走 text/hex 启发式；Office/PDF/媒体/表格/演示文稿/OFD/XMind/notebook 与
// tiff/heic 等需解码的图片交给 FileViewerPreview。读取仍受 files/read 的 2 MiB 上限约束。
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { Download, Minus, Pencil, Settings2, X } from "@lucide/vue";
import { baseName, call, errorMessage, formatBytes, isMethodMissing } from "../lib/api";
import { canEditBytes, hexDump, READ_MAX_BYTES, WRITE_MAX_BYTES } from "../lib/preview";
import { resolvePreview, type PreviewResolution } from "../lib/previewResolver";
import FileViewerPreview from "./FileViewerPreview.vue";
import TextPreview from "./TextPreview.vue";

const props = defineProps<{
  path: string | null;
  canWrite: boolean;
  /** 预览所属栏的连接 id（双栏左栏可为本地 __local__）；缺省走默认注入。 */
  connectionId?: string;
  /** 宿主外观（CodeMirror 主题色板，与 ssh sftp 编辑器同方案）。 */
  appearance: DbxPluginAppearance;
  /** 浮窗宿主（App 预览层）允许最小化成悬浮 pill 时显示按钮。 */
  allowMinimize?: boolean;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "saved", path: string): void;
  (event: "download", path: string): void;
  (event: "minimize"): void;
  /** viewer 无法渲染（unsupported/失败）时请求打开「打开方式」设置页。 */
  (event: "open-settings"): void;
}>();

type PreviewMode = "text" | "image" | "hex" | "archive" | "viewer";

const loading = ref(false);
const saving = ref(false);
const error = ref("");
const editError = ref("");
const truncated = ref(false);
const size = ref(0);
const mode = ref<PreviewMode>("text");
const editing = ref(false);
const text = ref("");
const draft = ref("");
const dataUri = ref("");
const hex = ref("");
const sourceFile = ref<File | null>(null);
const resolution = ref<PreviewResolution | null>(null);

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);
const title = computed(() => (props.path ? baseName(props.path) : ""));
const canEdit = computed(() => Boolean(
  resolution.value?.editable
  && props.canWrite
  && !truncated.value
  && !loading.value
  && !error.value
  && mode.value === "text"
  && !editing.value,
));
const showEditbar = computed(() => Boolean(
  resolution.value?.editable
  && props.canWrite
  && !truncated.value
  && !loading.value
  && !error.value
  && mode.value === "text",
));

// P1-4/R5-P2-6：打开时把焦点移入预览，卸载时归还触发元素。
const previewEl = ref<HTMLElement>();
const textPreviewRef = ref<InstanceType<typeof TextPreview>>();
let returnFocusTo: HTMLElement | null = null;

onMounted(async () => {
  returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  await nextTick();
  previewEl.value?.focus();
});

onUnmounted(() => {
  if (returnFocusTo?.isConnected) returnFocusTo.focus();
  returnFocusTo = null;
});

watch(editing, async (on) => {
  if (!on) return;
  await nextTick();
  textPreviewRef.value?.focus();
});

function withConnection(params: Record<string, unknown>): Record<string, unknown> {
  return props.connectionId ? { ...params, connectionId: props.connectionId } : params;
}

async function load() {
  if (!props.path) return;
  loading.value = true;
  saving.value = false;
  editing.value = false;
  error.value = "";
  editError.value = "";
  dataUri.value = "";
  hex.value = "";
  sourceFile.value = null;
  resolution.value = resolvePreview(props.path);
  text.value = "";
  draft.value = "";
  size.value = 0;
  truncated.value = false;
  try {
    if (resolution.value.previewStrategy === "archive") {
      // 压缩包内容列表：files/archiveList 分页渲染（B-ARCHIVE 既有链路）。
      mode.value = "archive";
      size.value = 0;
      truncated.value = false;
      await loadArchivePage(true);
      return;
    }
    const result = await call<{ dataBase64: string; truncated?: boolean; size?: number }>("files/read", withConnection({
      path: props.path,
      maxBytes: READ_MAX_BYTES,
    }));
    const bytes = window.dbxPlugin.decodeBase64(result.dataBase64);
    truncated.value = Boolean(result.truncated);
    size.value = result.size ?? bytes.byteLength;
    if (resolution.value.previewStrategy === "image") {
      mode.value = "image";
      dataUri.value = `data:${resolution.value.mime};base64,${result.dataBase64}`;
      return;
    }
    if (resolution.value.previewStrategy === "file-viewer") {
      mode.value = "viewer";
      const buffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
      sourceFile.value = new File([buffer], baseName(props.path), {
        type: resolution.value.mime ?? "application/octet-stream",
      });
      return;
    }
    // 文本/代码与未知扩展：先按文本解码，不可打印占比高时回退 hex dump。
    const decoded = new TextDecoder().decode(bytes);
    const printable = decoded.replace(/[^\t\n\r\x20-\x7E\u00A0-\uFFFF]/g, "");
    if (resolution.value.previewStrategy === "hex" && decoded.length && 1 - printable.length / decoded.length > 0.08) {
      mode.value = "hex";
      hex.value = hexDump(bytes.subarray(0, 512));
      return;
    }
    mode.value = "text";
    text.value = decoded;
    draft.value = decoded;
  } catch (cause) {
    error.value = errorMessage(cause);
  } finally {
    loading.value = false;
  }
}

function handleViewerError(cause: unknown) {
  error.value = errorMessage(cause);
}

function startEdit() {
  if (!canEdit.value) return;
  draft.value = text.value;
  editError.value = "";
  editing.value = true;
}

function cancelEdit() {
  draft.value = text.value;
  editError.value = "";
  editing.value = false;
}

async function saveEdit() {
  if (!props.path || saving.value) return;
  editError.value = "";
  const bytes = new TextEncoder().encode(draft.value);
  if (!canEditBytes(bytes.byteLength)) {
    editError.value = t("editTooLarge", { size: formatBytes(WRITE_MAX_BYTES) });
    return;
  }
  saving.value = true;
  try {
    await call("files/write", withConnection({ path: props.path, dataBase64: window.dbxPlugin.encodeBase64(bytes) }));
    text.value = draft.value;
    size.value = bytes.byteLength;
    editing.value = false;
    emit("saved", props.path);
    await load();
  } catch (cause) {
    editError.value = isMethodMissing(cause) ? t("featureMissing", { method: "files/write" }) : errorMessage(cause);
  } finally {
    saving.value = false;
  }
}

// --- 压缩包内容列表（files/archiveList 分页） ---

interface ArchiveEntry {
  name: string;
  path: string;
  kind: string;
  size?: number;
}

const ARCHIVE_PAGE_SIZE = 200;
const archiveEntries = ref<ArchiveEntry[]>([]);
const archiveTotal = ref(0);
const archivePage = ref(0);
const archiveDone = ref(false);
const archiveError = ref("");

async function loadArchivePage(reset: boolean) {
  if (!props.path) return;
  if (reset) {
    archiveEntries.value = [];
    archiveTotal.value = 0;
    archivePage.value = 0;
    archiveDone.value = false;
    archiveError.value = "";
  }
  if (archiveDone.value) return;
  const page = archivePage.value + 1;
  try {
    const result = await call<{ entries: ArchiveEntry[]; total?: number }>("files/archiveList", withConnection({
      path: props.path,
      page,
      pageSize: ARCHIVE_PAGE_SIZE,
    }));
    archivePage.value = page;
    archiveEntries.value.push(...(result.entries ?? []));
    archiveTotal.value = result.total ?? archiveEntries.value.length;
    archiveDone.value = archiveEntries.value.length >= archiveTotal.value || (result.entries ?? []).length === 0;
  } catch (cause) {
    if (isMethodMissing(cause)) {
      // 后端未含 archiveList（旧 sidecar）→ 保持原占位文案
      archiveError.value = t("archivePreviewUnsupported");
    } else {
      archiveError.value = errorMessage(cause);
    }
    archiveDone.value = true;
  }
}

watch(
  () => props.path,
  () => {
    if (props.path) void load();
  },
  { immediate: true },
);

// 审计#7：向外暴露未保存草稿状态——App 侧关闭预览（Esc/遮罩/关闭钮）时，
// 编辑中且草稿已改动先弹丢弃确认，防止静默丢失 CodeMirror 编辑内容。
const isDirty = computed(() => editing.value && draft.value !== text.value);
defineExpose({ isDirty });

// viewer 渲染不了的文档类格式（pptx 等未装配 renderer / 损坏文件）：提示改用
// 平台外部应用（WPS/Excel/...，打开方式设置里按 OS 预设一键配置）。
const canOpenExternal = computed(() =>
  resolution.value?.kind === "office" || resolution.value?.kind === "pdf" || resolution.value?.kind === "media",
);
</script>

<template>
  <div v-if="path" ref="previewEl" tabindex="-1" class="wb-preview">
    <div class="wb-preview-header">
      <strong :title="path">{{ title }}</strong>
      <span class="wb-muted">{{ loading || mode === "image" || mode === "archive" || mode === "viewer" ? "" : formatBytes(size) }}</span>
      <button v-if="canEdit && !saving" class="wb-icon-button wb-icon-neutral" v-tip="t('edit')" @click="startEdit"><Pencil /></button>
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('download')" @click="emit('download', path)"><Download /></button>
      <button v-if="allowMinimize" class="wb-icon-button wb-icon-neutral" v-tip="t('minimizePreview')" :aria-label="t('minimizePreview')" @click="emit('minimize')"><Minus /></button>
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="emit('close')"><X /></button>
    </div>
    <div v-if="showEditbar" class="wb-preview-editbar">
      <template v-if="editing">
        <span v-if="editError" class="wb-preview-edit-error">{{ editError }}</span>
        <button class="wb-toolbar-button" :disabled="saving" @click="cancelEdit">{{ t("cancel") }}</button>
        <button class="wb-toolbar-button wb-edit-save" :disabled="saving" @click="saveEdit">{{ t("save") }}</button>
      </template>
      <span v-else class="wb-muted">{{ t("edit") }}</span>
    </div>
    <div class="wb-preview-body" :aria-busy="loading">
      <template v-if="loading">
        <div class="wb-preview-skeleton">
          <span role="status" class="wb-muted">{{ t("loading") }}</span>
          <span v-for="row in 8" :key="row" class="wb-skeleton" aria-hidden="true" :style="{ width: `${100 - (row % 3) * 12}%` }" />
        </div>
      </template>
      <div v-else-if="error" class="wb-preview-notice">
        <div>{{ t("previewLoadError", { error }) }}</div>
        <div v-if="canOpenExternal" class="wb-muted">{{ t("previewExternalHint") }}</div>
        <div class="wb-preview-notice-actions">
          <button v-if="canOpenExternal" class="wb-toolbar-button" @click="emit('open-settings')"><Settings2 /> {{ t("openInSettings") }}</button>
          <button class="wb-toolbar-button" @click="emit('download', path)"><Download /> {{ t("download") }}</button>
        </div>
      </div>
      <img v-else-if="mode === 'image'" :src="dataUri" :alt="title" />
      <!-- 文本预览/编辑：CodeMirror（与 ssh sftp 面板同方案）；key 保证
           进入/退出编辑都从 props.text 全新装载，取消编辑即回滚草稿。 -->
      <TextPreview
        v-else-if="mode === 'text'"
        ref="textPreviewRef"
        :key="editing ? 'edit' : 'read'"
        :text="text"
        :file-name="path"
        :appearance="appearance"
        :editable="editing"
        @change="draft = $event"
      />
      <pre v-else-if="mode === 'hex'" class="wb-mono wb-hex">{{ hex }}</pre>
      <div v-else-if="mode === 'archive'" class="wb-archive">
        <div v-if="archiveError" class="wb-preview-notice">{{ archiveError }}</div>
        <template v-else>
          <div class="wb-muted wb-archive-total">{{ t("archiveEntriesLabel", { count: archiveTotal }) }}</div>
          <ul class="wb-archive-list wb-mono">
            <li v-for="entry in archiveEntries" :key="entry.path" class="wb-archive-entry">
              <span class="wb-archive-kind">{{ entry.kind === "directory" ? "🗂" : "📄" }}</span>
              <span class="wb-archive-path" :title="entry.path">{{ entry.path }}</span>
              <span class="wb-muted">{{ formatBytes(entry.size) }}</span>
            </li>
          </ul>
          <div v-if="!archiveDone" class="wb-archive-more">
            <button class="wb-toolbar-button" :disabled="loading" @click="loadArchivePage(false)">
              {{ t("archiveLoadMore") }}
            </button>
          </div>
        </template>
      </div>
      <FileViewerPreview
        v-else-if="mode === 'viewer' && sourceFile"
        :source="sourceFile"
        :file-name="title"
        :mime="resolution?.mime ?? undefined"
        :appearance="appearance"
        class="wb-file-viewer-preview"
        @error="handleViewerError"
      />
      <div v-else class="wb-preview-notice">{{ t("previewUnsupported") }}</div>
    </div>
    <div v-if="truncated" class="wb-notice">
      <span>{{ t("previewTruncated", { size: formatBytes(READ_MAX_BYTES) }) }}</span>
      <button class="wb-toolbar-button" @click="emit('download', path)"><Download /> {{ t("download") }}</button>
    </div>
  </div>
</template>
