<script lang="ts">
// Markdown 渲染/源码切换的会话级记忆：script-setup 顶层是实例作用域，
// 会话记忆须挂模块作用域（同会话多次打开预览沿用上次选择，不持久化）。
export type MarkdownView = "render" | "source";
let sessionMarkdownView: MarkdownView = "render";
</script>

<script setup lang="ts">
// 预览两套方案：文本/代码走 CodeMirror，图片走原生元素，归档走 files/archiveList，
// 未知扩展走 text/hex 启发式；Office/PDF/媒体/表格/演示文稿/OFD/XMind/notebook 与
// tiff/heic 等需解码的图片交给 FileViewerPreview。二进制预览经 files/stat +
// files/readRange 分块流式拼装（2MiB/片，≤256MiB）；文本/可编辑链路仍走 files/read（≤2MiB）。
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { Download, Minus, Pencil, RefreshCw, Settings2, X } from "@lucide/vue";
import { baseName, call, errorMessage, formatBytes, isMethodMissing } from "../lib/api";
import { canEditBytes, hexDump, PREVIEW_MAX_BYTES, READ_MAX_BYTES, WRITE_MAX_BYTES } from "../lib/preview";
import { resolvePreview, type PreviewResolution } from "../lib/previewResolver";
import FileViewerPreview from "./FileViewerPreview.vue";
import MarkdownRender from "./MarkdownRender.vue";
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
// 分块预览超硬上限（>256MiB 不进浏览器内存，保持「下载代替」出口）。
const overCap = ref(false);
const size = ref(0);
const mode = ref<PreviewMode>("text");
const editing = ref(false);
const text = ref("");
const draft = ref("");
const dataUri = ref("");
const hex = ref("");
const sourceFile = ref<File | null>(null);
const resolution = ref<PreviewResolution | null>(null);

// Markdown 渲染视图：md 文件默认渲染面，头部「渲染/源码」分段切换。
// 选择存会话级记忆（不持久化），新开文件沿用上次选择；解析异常回退源码。
const mdView = ref<MarkdownView>(sessionMarkdownView);
const isMarkdown = computed(() => resolution.value?.mime === "text/markdown");
const showMdToggle = computed(() => Boolean(
  isMarkdown.value
  && mode.value === "text"
  && !editing.value
  && !loading.value
  && !error.value,
));
function setMarkdownView(view: MarkdownView) {
  mdView.value = view;
  sessionMarkdownView = view;
}
// i18n key（mdRenderView/mdSourceView）由主协调者七语补录；workbenchMessage
// 缺 key 时返回 key 本身，据此回退到组件内置兜底文案，key 入库后自动生效。
function mdLabel(key: string, fallback: string): string {
  const value = t(key);
  return value === key ? fallback : value;
}

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
  mdView.value = sessionMarkdownView;
  text.value = "";
  draft.value = "";
  size.value = 0;
  truncated.value = false;
  overCap.value = false;
  // 换文件即失效进行中的分块循环（token 检查点见 runChunkLoop）。
  loadToken += 1;
  chunkCancelRequested.value = false;
  chunkActive.value = false;
  loadedBytes.value = 0;
  totalBytes.value = 0;
  parts = [];
  revokeObjectUrl();
  try {
    if (resolution.value.previewStrategy === "archive") {
      // 压缩包内容列表：files/archiveList 分页渲染（B-ARCHIVE 既有链路）。
      mode.value = "archive";
      size.value = 0;
      truncated.value = false;
      await loadArchivePage(true);
      return;
    }
    // 二进制预览（图片/媒体/Office/PDF）：stat 取大小 → readRange 分块流式拼装；
    // 文本/代码与未知扩展仍走 files/read（可编辑链路不动）。
    if (resolution.value.previewStrategy === "image" || resolution.value.previewStrategy === "file-viewer") {
      await loadChunked(resolution.value);
      return;
    }
    const result = await call<{ dataBase64: string; truncated?: boolean; size?: number }>("files/read", withConnection({
      path: props.path,
      maxBytes: READ_MAX_BYTES,
    }));
    const bytes = window.dbxPlugin.decodeBase64(result.dataBase64);
    truncated.value = Boolean(result.truncated);
    size.value = result.size ?? bytes.byteLength;
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

// --- 二进制预览分块流式加载（files/stat + files/readRange）--------------------
// 有效预览上限 2MiB → 256MiB：按 2MiB/片顺序拉取（并发 1）拼装成单个 Blob，
// 进度条以「已取字节 / totalSize」渲染；取消在分片间隙打断并干净关闭；
// 失败保留已取字节，可从失败 offset 续传。eof / 取满 totalSize 即收口。

const chunkActive = ref(false);
const chunkCancelRequested = ref(false);
const loadedBytes = ref(0);
const totalBytes = ref(0);
const chunkPercent = computed(() => {
  if (!totalBytes.value) return 0;
  return Math.min(100, Math.round((loadedBytes.value / totalBytes.value) * 100));
});
/** 分块循环的失效令牌：换文件/取消时自增，旧循环在下一个检查点退出。 */
let loadToken = 0;
/** 已取分片（非响应式：大数组逐片 push 不应触发渲染）。 */
let parts: Uint8Array[] = [];
let objectUrl = "";

function revokeObjectUrl() {
  if (objectUrl) {
    URL.revokeObjectURL(objectUrl);
    objectUrl = "";
  }
}

onUnmounted(revokeObjectUrl);

function concatParts(chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

async function loadChunked(resolution: PreviewResolution) {
  const stat = await call<{ entry?: { size?: number }; size?: number }>("files/stat", withConnection({
    path: props.path,
  }));
  const total = Number(stat.entry?.size ?? stat.size ?? 0);
  size.value = total;
  totalBytes.value = total;
  if (total > PREVIEW_MAX_BYTES) {
    overCap.value = true;
    return;
  }
  chunkActive.value = true;
  parts = [];
  loadedBytes.value = 0;
  await runChunkLoop(resolution, 0);
}

/** 分片循环：顺序（并发 1）拉取；token/取消在每个 await 后检查。 */
async function runChunkLoop(resolution: PreviewResolution, fromOffset: number) {
  const token = loadToken;
  try {
    let offset = fromOffset;
    const total = () => totalBytes.value;
    while (offset < total()) {
      if (token !== loadToken || chunkCancelRequested.value) return;
      const length = Math.min(READ_MAX_BYTES, total() - offset);
      const range = await call<{ dataBase64: string; totalSize: number; offset: number; eof: boolean }>(
        "files/readRange",
        withConnection({ path: props.path, offset, length }),
      );
      if (token !== loadToken || chunkCancelRequested.value) return;
      const chunk = window.dbxPlugin.decodeBase64(range.dataBase64);
      parts.push(chunk);
      // 0 字节响应按请求长度推进，避免坏实现卡死循环。
      offset += chunk.byteLength || length;
      loadedBytes.value = offset;
      // 契约收口：eof 或已取满 totalSize（totalSize 以响应为准，容错修正）。
      if (range.eof) break;
      if (range.totalSize && range.totalSize !== totalBytes.value) {
        totalBytes.value = range.totalSize;
        if (offset >= range.totalSize) break;
      }
    }
    finishChunkPreview(resolution);
  } catch (cause) {
    if (token !== loadToken || chunkCancelRequested.value) return;
    // 旧 sidecar 无 readRange：小文件回退原 files/read 整读路径。
    if (isMethodMissing(cause) && fromOffset === 0 && totalBytes.value <= READ_MAX_BYTES) {
      await loadViaRead(resolution);
      return;
    }
    // 保留已取字节与 chunkActive：错误面板给出「从失败 offset 重试」。
    error.value = errorMessage(cause);
  }
}

/** 旧 sidecar 回退：readRange 缺失且文件 ≤2MiB 时按原整读路径拼装。 */
async function loadViaRead(resolution: PreviewResolution) {
  const result = await call<{ dataBase64: string; truncated?: boolean; size?: number }>("files/read", withConnection({
    path: props.path,
    maxBytes: READ_MAX_BYTES,
  }));
  const bytes = window.dbxPlugin.decodeBase64(result.dataBase64);
  truncated.value = Boolean(result.truncated);
  size.value = result.size ?? bytes.byteLength;
  parts = [bytes];
  loadedBytes.value = bytes.byteLength;
  finishChunkPreview(resolution);
}

/** 拼装收口：image → object URL（dataUri 直用）；viewer → File 交给既有渲染。 */
function finishChunkPreview(resolution: PreviewResolution) {
  chunkActive.value = false;
  const blob = new Blob(parts as BlobPart[], { type: resolution.mime ?? "application/octet-stream" });
  if (resolution.previewStrategy === "image") {
    mode.value = "image";
    if (typeof URL !== "undefined" && typeof URL.createObjectURL === "function") {
      revokeObjectUrl();
      objectUrl = URL.createObjectURL(blob);
      dataUri.value = objectUrl;
    } else if (typeof window.dbxPlugin.encodeBase64 === "function") {
      // 极旧宿主无 createObjectURL：回退 data URI（内存翻倍，仅兜底）。
      dataUri.value = `data:${resolution.mime};base64,${window.dbxPlugin.encodeBase64(concatParts(parts))}`;
    }
    return;
  }
  mode.value = "viewer";
  sourceFile.value = new File([blob], baseName(props.path ?? ""), { type: blob.type });
}

/** 取消：打断剩余分片并干净关闭（不落错误 UI）。 */
function cancelChunkLoad() {
  if (!chunkActive.value) return;
  chunkCancelRequested.value = true;
  chunkActive.value = false;
  parts = [];
  emit("close");
}

/** 失败续传：从已取 offset 继续拉剩余分片（沿用原 resolution 渲染链路）。 */
async function resumeChunkLoad() {
  if (!props.path || !resolution.value || loading.value) return;
  error.value = "";
  chunkCancelRequested.value = false;
  chunkActive.value = true;
  loading.value = true;
  try {
    await runChunkLoop(resolution.value, Math.min(loadedBytes.value, totalBytes.value));
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
      <!-- Markdown 渲染/源码切换（仅 md 文本态出现；i18n key 七语补录前用兜底文案）。 -->
      <span v-if="showMdToggle" class="wb-md-toggle" data-test="md-toggle">
        <button
          class="wb-toolbar-button"
          :class="{ 'wb-md-toggle-active': mdView === 'render' }"
          data-test="md-render"
          :title="`${mdLabel('mdRenderView', '渲染')} / Rendered view`"
          :aria-pressed="mdView === 'render'"
          @click="setMarkdownView('render')"
        >{{ mdLabel("mdRenderView", "渲染") }}</button>
        <button
          class="wb-toolbar-button"
          :class="{ 'wb-md-toggle-active': mdView === 'source' }"
          data-test="md-source"
          :title="`${mdLabel('mdSourceView', '源码')} / Markdown source`"
          :aria-pressed="mdView === 'source'"
          @click="setMarkdownView('source')"
        >{{ mdLabel("mdSourceView", "源码") }}</button>
      </span>
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
    <!-- 分块加载进度（二进制预览）：字节进度 + 取消；失败后保留以便续传重试。 -->
    <div v-if="chunkActive" class="wb-preview-chunkbar" data-test="chunkbar">
      <span class="wb-muted">{{ t("previewChunkProgress", { done: formatBytes(loadedBytes), total: formatBytes(totalBytes) }) }}</span>
      <div
        class="wb-progress"
        role="progressbar"
        :aria-label="t('previewChunkProgress', { done: formatBytes(loadedBytes), total: formatBytes(totalBytes) })"
        aria-valuemin="0"
        aria-valuemax="100"
        :aria-valuenow="chunkPercent"
      ><div class="wb-progress-bar" :style="{ width: `${chunkPercent}%` }" /></div>
      <button class="wb-toolbar-button" data-test="chunk-cancel" @click="cancelChunkLoad">{{ t("cancel") }}</button>
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
          <!-- 分块失败续传：从失败 offset 继续（>0 才有意义）。 -->
          <button v-if="chunkActive && loadedBytes" class="wb-toolbar-button" data-test="chunk-retry" @click="resumeChunkLoad"><RefreshCw /> {{ t("previewRetryFromOffset", { offset: loadedBytes }) }}</button>
          <button v-if="canOpenExternal" class="wb-toolbar-button" @click="emit('open-settings')"><Settings2 /> {{ t("openInSettings") }}</button>
          <button class="wb-toolbar-button" @click="emit('download', path)"><Download /> {{ t("download") }}</button>
        </div>
      </div>
      <img v-else-if="mode === 'image'" :src="dataUri" :alt="title" />
      <!-- Markdown 渲染视图：白名单子集 + 全量 HTML 转义（MarkdownRender.vue）；
           解析异常经 render-error 回退源码（编辑态恒走 CodeMirror）。 -->
      <MarkdownRender
        v-else-if="mode === 'text' && showMdToggle && mdView === 'render'"
        :text="text"
        @render-error="setMarkdownView('source')"
      />
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
    <!-- 分块预览超硬上限（>256MiB）：不进浏览器内存，保持「下载代替」出口。 -->
    <div v-if="overCap" class="wb-notice" data-test="overcap">
      <span>{{ t("previewOverCap", { size: formatBytes(PREVIEW_MAX_BYTES) }) }}</span>
      <button class="wb-toolbar-button" @click="emit('download', path)"><Download /> {{ t("download") }}</button>
    </div>
  </div>
</template>
