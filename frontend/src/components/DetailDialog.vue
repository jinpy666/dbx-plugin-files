<script setup lang="ts">
// 文件详情对话框（issue #98 只读先行 + #100 metadata 编辑）：files/info 的
// 展示面。基础区恒显；ETag / user metadata 表 / POSIX（mode/uid/gid）区按
// 后端能力有则显示，全缺时给优雅降级提示（非对象存储后端正常态，不算
// 错误）。issue #100：canEditMetadata（后端 files/capabilities.metadataWrite）
// 时额外给「编辑元数据」入口——编辑 content-type 与 x-amz-meta-* 键值，
// 提交经 files/setMetadata 两跳改写（不重传对象）。Esc / 关闭按钮 / 点背景
// 空白处关闭，复用全局 wb-dialog 样式族。
import { computed, nextTick, ref, watch } from "vue";
import { formatBytes, formatTime, type FileEntry } from "../lib/api";
import { trapTabKey } from "../lib/a11y";
import {
  draftFromDetail,
  draftToWireMap,
  type MetadataBuildReason,
  type MetadataDraftRow,
} from "../lib/metadataEdit";

export interface DetailInfo {
  mimeType?: string;
  etag?: string;
  hashes?: Record<string, string>;
  mode?: string;
  uid?: string;
  gid?: string;
  metadata?: { key: string; value: string }[];
}

export type DetailSaveState = "idle" | "saving" | "saved" | "error";

const props = defineProps<{
  open: boolean;
  entry: FileEntry | null;
  detail: DetailInfo | null;
  t: (key: string, values?: Record<string, string | number>) => string;
  /** 后端 files/capabilities.metadataWrite 且连接可写时才显示编辑入口。 */
  canEditMetadata?: boolean;
  /** 父层提交状态；saved 时退出编辑态（后端错误由 showError 统一展示）。 */
  saveState?: DetailSaveState;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "save", metadata: Record<string, string>): void;
}>();

const dialogEl = ref<HTMLElement>();
let returnFocusTo: HTMLElement | null = null;

const editing = ref(false);
const draftContentType = ref("");
const draftRows = ref<MetadataDraftRow[]>([]);
const draftError = ref<{ key: string; values?: Record<string, string | number> } | null>(null);
const saving = computed(() => props.saveState === "saving");
const draftErrorText = computed(() =>
  draftError.value ? props.t(draftError.value.key, draftError.value.values) : "",
);

// 打开时焦点进对话框、关闭归还触发元素（对齐 DropActionDialog 的焦点链路）。
watch(
  () => props.open,
  async (open) => {
    if (open) {
      returnFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      await nextTick();
      if (!props.open) return; // 同帧内又被关闭：不抢焦点
      dialogEl.value?.querySelector<HTMLElement>(".wb-detail-close")?.focus();
      return;
    }
    editing.value = false;
    draftError.value = null;
    returnFocusTo?.focus();
    returnFocusTo = null;
  },
  { immediate: true },
);

// 父层刷新完成 → 退出编辑态（saved 由 success 路径设置，error 保持编辑）。
watch(
  () => props.saveState,
  (state) => {
    if (state === "saved") {
      editing.value = false;
      draftError.value = null;
    }
  },
);

watch(
  () => props.canEditMetadata,
  (enabled) => {
    if (!enabled) editing.value = false;
  },
);

function beginEdit() {
  const seeded = draftFromDetail(props.detail);
  draftContentType.value = seeded.contentType;
  draftRows.value = seeded.rows.map((row) => ({ ...row }));
  draftError.value = null;
  editing.value = true;
  void nextTick(() => {
    dialogEl.value?.querySelector<HTMLElement>(".wb-metadata-input")?.focus();
  });
}

function cancelEdit() {
  editing.value = false;
  draftError.value = null;
}

function addMetadataRow() {
  draftRows.value.push({ key: "", value: "" });
}

function removeMetadataRow(index: number) {
  draftRows.value.splice(index, 1);
}

function reasonMessage(reason: MetadataBuildReason, key?: string) {
  switch (reason) {
    case "emptyValue":
      return { key: "metadataEmptyValue", values: { key: key ?? "" } };
    case "invalidKey":
      return { key: "metadataInvalidKey", values: { key: key ?? "" } };
    case "duplicateKey":
      return { key: "metadataDuplicateKey", values: { key: key ?? "" } };
    case "invalidValue":
      return { key: "metadataInvalidValue", values: { key: key ?? "" } };
    case "noChanges":
    default:
      return { key: "metadataNoChanges" };
  }
}

function submitEdit() {
  const result = draftToWireMap(draftFromDetail(props.detail), {
    contentType: draftContentType.value,
    rows: draftRows.value.map((row) => ({ ...row })),
  });
  if (!result.ok) {
    draftError.value = reasonMessage(result.reason, result.key);
    return;
  }
  draftError.value = null;
  emit("save", result.metadata);
}

const sizeText = computed(() =>
  props.entry?.kind === "file" ? formatBytes(props.entry.size) : "",
);
const timeText = computed(() => formatTime(props.entry?.modifiedAt));
const metadataRows = computed(() => props.detail?.metadata ?? []);
const hashRows = computed(() => Object.entries(props.detail?.hashes ?? {}));
const hasPosix = computed(() => Boolean(props.detail?.mode || props.detail?.uid || props.detail?.gid));
const hasExtended = computed(() =>
  Boolean(props.detail?.etag || hasPosix.value || metadataRows.value.length || hashRows.value.length),
);

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") {
    event.preventDefault();
    // 编辑态 Esc 先退出编辑（不丢草稿式关闭对话框）；再按一次才关闭。
    if (editing.value) {
      cancelEdit();
      return;
    }
    emit("close");
    return;
  }
  trapTabKey(event, dialogEl.value);
}
</script>

<template>
  <div v-if="open && entry" class="wb-dialog-backdrop" @click.self="emit('close')">
    <div ref="dialogEl" class="wb-dialog wb-detail" role="dialog" aria-modal="true" :aria-label="t('detailTitle')" @keydown="onKeydown">
      <header>{{ t("detailTitle") }}</header>
      <div class="wb-dialog-body">
        <div class="wb-detail-section">
          <div class="wb-detail-row"><span>{{ t("detailName") }}</span><strong>{{ entry.name }}</strong></div>
          <div class="wb-detail-row"><span>{{ t("detailPath") }}</span><span class="wb-detail-path">{{ entry.path }}</span></div>
          <div class="wb-detail-row"><span>{{ t("detailKind") }}</span><span>{{ entry.kind === "directory" ? t("detailKindDir") : t("detailKindFile") }}</span></div>
          <div v-if="entry.kind === 'file'" class="wb-detail-row"><span>{{ t("detailSize") }}</span><span>{{ sizeText }}</span></div>
          <div v-if="timeText" class="wb-detail-row"><span>{{ t("detailModified") }}</span><span>{{ timeText }}</span></div>
          <div v-if="detail?.mimeType" class="wb-detail-row"><span>{{ t("detailMimeType") }}</span><span>{{ detail.mimeType }}</span></div>
        </div>
        <div v-if="detail?.etag" class="wb-detail-section">
          <div class="wb-detail-row"><span>{{ t("detailEtag") }}</span><span class="wb-detail-path">{{ detail.etag }}</span></div>
        </div>
        <div v-if="hashRows.length" class="wb-detail-section">
          <div class="wb-detail-heading">{{ t("detailHashes") }}</div>
          <table class="wb-detail-meta">
            <tbody>
              <tr v-for="[algorithm, digest] in hashRows" :key="algorithm" class="wb-detail-hash-row">
                <th scope="row">{{ algorithm }}</th>
                <td>{{ digest }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <div v-if="hasPosix" class="wb-detail-section wb-detail-posix">
          <div v-if="detail?.mode" class="wb-detail-row"><span>{{ t("detailMode") }}</span><span>{{ detail.mode }}</span></div>
          <div v-if="detail?.uid" class="wb-detail-row"><span>{{ t("detailUid") }}</span><span>{{ detail.uid }}</span></div>
          <div v-if="detail?.gid" class="wb-detail-row"><span>{{ t("detailGid") }}</span><span>{{ detail.gid }}</span></div>
        </div>
        <div v-if="editing" class="wb-detail-section wb-detail-edit" data-test="metadata-editor">
          <label class="wb-detail-edit-field">
            <span>{{ t("detailMimeType") }}</span>
            <input
              v-model="draftContentType"
              class="wb-metadata-input"
              type="text"
              :disabled="saving"
              data-test="metadata-content-type"
            />
          </label>
          <div class="wb-detail-heading">{{ t("metadataUserKeys") }}</div>
          <div v-for="(row, index) in draftRows" :key="index" class="wb-detail-edit-row">
            <input
              v-model="row.key"
              class="wb-metadata-input"
              type="text"
              :placeholder="t('metadataKeyPlaceholder')"
              :aria-label="t('metadataKeyPlaceholder')"
              :disabled="saving"
              data-test="metadata-key"
            />
            <input
              v-model="row.value"
              class="wb-metadata-input"
              type="text"
              :placeholder="t('metadataValuePlaceholder')"
              :aria-label="t('metadataValuePlaceholder')"
              :disabled="saving"
              data-test="metadata-value"
            />
            <button
              type="button"
              class="wb-detail-edit-remove"
              :aria-label="t('metadataRemoveRow')"
              :title="t('metadataRemoveRow')"
              :disabled="saving"
              @click="removeMetadataRow(index)"
            >
              ×
            </button>
          </div>
          <button type="button" class="wb-detail-add-row" :disabled="saving" @click="addMetadataRow">
            {{ t("metadataAddRow") }}
          </button>
          <p class="wb-detail-hint">{{ t("metadataEditHint") }}</p>
          <p v-if="draftErrorText" class="wb-detail-error" role="alert" data-test="metadata-error">{{ draftErrorText }}</p>
        </div>
        <div v-else-if="metadataRows.length" class="wb-detail-section">
          <div class="wb-detail-heading">
            <span>{{ t("detailMetadata") }}</span>
            <button
              v-if="canEditMetadata && entry.kind === 'file'"
              type="button"
              class="wb-detail-edit-button"
              data-test="metadata-edit"
              @click="beginEdit"
            >
              {{ t("metadataEdit") }}
            </button>
          </div>
          <table class="wb-detail-meta">
            <tbody>
              <tr v-for="row in metadataRows" :key="row.key" class="wb-detail-meta-row">
                <th scope="row">{{ row.key }}</th>
                <td>{{ row.value }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <div v-else-if="canEditMetadata && entry.kind === 'file'" class="wb-detail-section">
          <div class="wb-detail-heading">
            <span>{{ t("detailMetadata") }}</span>
            <button type="button" class="wb-detail-edit-button" data-test="metadata-edit" @click="beginEdit">
              {{ t("metadataEdit") }}
            </button>
          </div>
        </div>
        <p v-if="!hasExtended && !editing && !canEditMetadata" class="wb-detail-empty">{{ t("detailNoMetadata") }}</p>
      </div>
      <footer>
        <template v-if="editing">
          <button type="button" :disabled="saving" @click="cancelEdit">{{ t("cancel") }}</button>
          <button type="button" class="wb-dialog-primary" :disabled="saving" data-test="metadata-save" @click="submitEdit">
            {{ saving ? t("metadataSaving") : t("save") }}
          </button>
        </template>
        <button v-else type="button" class="wb-dialog-primary wb-detail-close" @click="emit('close')">{{ t("close") }}</button>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.wb-detail-section {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-bottom: 14px;
}
.wb-detail-row {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  font-size: 13px;
  line-height: 1.5;
}
.wb-detail-row > span:first-child {
  color: var(--text-dim, #888);
  flex-shrink: 0;
}
.wb-detail-row strong {
  font-weight: 600;
  word-break: break-all;
  text-align: right;
}
.wb-detail-path {
  word-break: break-all;
  text-align: right;
  user-select: text;
}
.wb-detail-heading {
  font-size: 12px;
  font-weight: 600;
  color: var(--text-dim, #888);
}
.wb-detail-meta {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}
.wb-detail-meta th {
  text-align: left;
  font-weight: 500;
  padding: 3px 12px 3px 0;
  color: var(--text-dim, #888);
  word-break: break-all;
}
.wb-detail-meta td {
  padding: 3px 0;
  word-break: break-all;
  user-select: text;
}
.wb-detail-empty {
  font-size: 12px;
  color: var(--text-dim, #888);
  margin: 0;
}
/* issue #100：metadata 编辑态（content-type + x-amz-meta-* 键值行）。 */
.wb-detail-heading {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.wb-detail-edit-button,
.wb-detail-add-row,
.wb-detail-edit-remove {
  background: transparent;
  border: 1px solid var(--border, #444);
  border-radius: 4px;
  color: inherit;
  cursor: pointer;
  font-size: 12px;
  padding: 2px 8px;
}
.wb-detail-edit-button:hover,
.wb-detail-add-row:hover,
.wb-detail-edit-remove:hover {
  background: var(--hover, rgba(127, 127, 127, 0.15));
}
.wb-detail-edit-field,
.wb-detail-edit-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.wb-detail-edit-field > span {
  color: var(--text-dim, #888);
  font-size: 12px;
  flex: 0 0 96px;
}
.wb-detail-edit-row .wb-metadata-input:first-child {
  flex: 0 0 40%;
}
.wb-detail-edit-row .wb-metadata-input:nth-child(2) {
  flex: 1 1 auto;
}
.wb-metadata-input {
  background: var(--input-bg, transparent);
  border: 1px solid var(--border, #444);
  border-radius: 4px;
  color: inherit;
  font: inherit;
  font-size: 12px;
  padding: 4px 6px;
  min-width: 0;
}
.wb-detail-edit-remove {
  flex: 0 0 auto;
  line-height: 1;
  padding: 4px 7px;
}
.wb-detail-add-row {
  align-self: flex-start;
}
.wb-detail-hint {
  color: var(--text-dim, #888);
  font-size: 11px;
  margin: 0;
}
.wb-detail-error {
  color: var(--danger, #e5484d);
  font-size: 12px;
  margin: 0;
}
</style>
