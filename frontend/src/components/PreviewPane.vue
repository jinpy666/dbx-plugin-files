<script setup lang="ts">
// 预览与编辑（A-FILES ②）：files/read（≤2MiB base64）。
// 文本可切换编辑（textarea）→ files/write（≤4MiB，超限引导走上传）；
// 图片按扩展名 data URI；二进制类前 512B hex dump；truncated 提示 + 下载引导；
// 压缩包列表需要后端 files/archiveList（见交接文档），先给占位说明。
import { computed, ref, watch } from "vue";
import { Download, Pencil, X } from "@lucide/vue";
import { baseName, call, errorMessage, formatBytes, isMethodMissing } from "../lib/api";
import { isArchivePath } from "../lib/archive";
import { canEditBytes, hexDump, imageMimeFor, READ_MAX_BYTES, WRITE_MAX_BYTES } from "../lib/preview";

const props = defineProps<{
  path: string | null;
  canWrite: boolean;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "saved", path: string): void;
  (event: "download", path: string): void;
}>();

type PreviewMode = "text" | "image" | "hex" | "binary" | "archive";

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

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

const title = computed(() => (props.path ? baseName(props.path) : ""));
/** truncated 时禁编辑：否则保存会把截断内容写回覆盖整个文件。 */
const canEdit = computed(() => mode.value === "text" && props.canWrite && !truncated.value && !loading.value && !error.value && !editing.value);
/** 编辑条：编辑态或可进入编辑态时显示。 */
const showEditbar = computed(() => mode.value === "text" && props.canWrite && !truncated.value && !loading.value && !error.value);

async function load() {
  if (!props.path) return;
  loading.value = true;
  saving.value = false;
  editing.value = false;
  error.value = "";
  editError.value = "";
  dataUri.value = "";
  hex.value = "";
  text.value = "";
  draft.value = "";
  try {
    if (isArchivePath(props.path)) {
      // 压缩包内容列表：files/archiveList 分页渲染（B-ARCHIVE 遗留③收口）。
      mode.value = "archive";
      size.value = 0;
      truncated.value = false;
      await loadArchivePage(true);
      return;
    }
    const mime = imageMimeFor(props.path);
    const result = await call<{ dataBase64: string; truncated?: boolean; size?: number }>("files/read", {
      path: props.path,
      maxBytes: READ_MAX_BYTES,
    });
    const bytes = window.dbxPlugin.decodeBase64(result.dataBase64);
    truncated.value = Boolean(result.truncated);
    size.value = result.size ?? bytes.byteLength;
    if (mime) {
      mode.value = "image";
      dataUri.value = `data:${mime};base64,${result.dataBase64}`;
      return;
    }
    const decoded = new TextDecoder().decode(bytes);
    const printable = decoded.replace(/[^\t\n\r\x20-\x7E\u00A0-\uFFFF]/g, "");
    // 不可打印字符占比高时视为二进制 → 前 512B hex dump
    if (decoded.length && 1 - printable.length / decoded.length > 0.08) {
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

function startEdit() {
  draft.value = text.value;
  editError.value = "";
  editing.value = true;
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
    const result = await call<{ entries: ArchiveEntry[]; total?: number }>("files/archiveList", {
      path: props.path,
      page,
      pageSize: ARCHIVE_PAGE_SIZE,
    });
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
    await call("files/write", { path: props.path, dataBase64: window.dbxPlugin.encodeBase64(bytes) });
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

watch(
  () => props.path,
  () => {
    if (props.path) void load();
  },
  // 右栏 Tab 切换时组件随挂载即带 path：必须 immediate 首载
  { immediate: true },
);
</script>

<template>
  <div v-if="path" class="wb-preview">
    <div class="wb-preview-header">
      <strong :title="path">{{ title }}</strong>
      <span class="wb-muted">{{ mode === "image" || mode === "archive" ? "" : formatBytes(size) }}</span>
      <button v-if="canEdit && !saving" class="wb-icon-button wb-icon-neutral" :title="t('edit')" @click="startEdit"><Pencil /></button>
      <button class="wb-icon-button wb-icon-neutral" :title="t('download')" @click="emit('download', path)"><Download /></button>
      <button class="wb-icon-button wb-icon-neutral" :title="t('close')" @click="emit('close')"><X /></button>
    </div>
    <div v-if="showEditbar" class="wb-preview-editbar">
      <template v-if="editing">
        <span v-if="editError" class="wb-preview-edit-error">{{ editError }}</span>
        <button class="wb-toolbar-button" :disabled="saving" @click="cancelEdit">{{ t("cancel") }}</button>
        <button class="wb-toolbar-button wb-edit-save" :disabled="saving" @click="saveEdit">{{ t("save") }}</button>
      </template>
      <span v-else class="wb-muted">{{ t("edit") }}</span>
    </div>
    <div class="wb-preview-body">
      <template v-if="loading">
        <div class="wb-preview-skeleton">
          <span v-for="row in 8" :key="row" class="wb-skeleton" :style="{ width: `${100 - (row % 3) * 12}%` }" />
        </div>
      </template>
      <div v-else-if="error" class="wb-preview-notice">{{ t("previewLoadError", { error }) }}</div>
      <img v-else-if="mode === 'image'" :src="dataUri" :alt="title" />
      <textarea
        v-else-if="mode === 'text' && editing"
        v-model="draft"
        class="wb-edit-area wb-mono"
        spellcheck="false"
        :disabled="saving"
      />
      <pre v-else-if="mode === 'text'">{{ text }}</pre>
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
      <div v-else class="wb-preview-notice">{{ t("previewUnsupported") }}</div>
    </div>
    <div v-if="truncated" class="wb-notice">
      <span>{{ t("previewTruncated", { size: formatBytes(READ_MAX_BYTES) }) }}</span>
      <button class="wb-toolbar-button" @click="emit('download', path)"><Download /> {{ t("download") }}</button>
    </div>
  </div>
</template>
