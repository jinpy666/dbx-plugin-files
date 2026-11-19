<script setup lang="ts">
// 文件详情对话框（issue #98 只读先行）：files/info 的展示面。基础区恒显；
// ETag / user metadata 表 / POSIX（mode/uid/gid）区按后端能力有则显示，
// 全缺时给优雅降级提示（非对象存储后端正常态，不算错误）。Esc / 关闭按钮
// / 点背景空白处关闭，复用全局 wb-dialog 样式族。
import { computed, nextTick, ref, watch } from "vue";
import { formatBytes, formatTime, type FileEntry } from "../lib/api";
import { trapTabKey } from "../lib/a11y";

export interface DetailInfo {
  mimeType?: string;
  etag?: string;
  hashes?: Record<string, string>;
  mode?: string;
  uid?: string;
  gid?: string;
  metadata?: { key: string; value: string }[];
}

const props = defineProps<{
  open: boolean;
  entry: FileEntry | null;
  detail: DetailInfo | null;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
}>();

const dialogEl = ref<HTMLElement>();
let returnFocusTo: HTMLElement | null = null;

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
    returnFocusTo?.focus();
    returnFocusTo = null;
  },
  { immediate: true },
);

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
        <div v-if="metadataRows.length" class="wb-detail-section">
          <div class="wb-detail-heading">{{ t("detailMetadata") }}</div>
          <table class="wb-detail-meta">
            <tbody>
              <tr v-for="row in metadataRows" :key="row.key" class="wb-detail-meta-row">
                <th scope="row">{{ row.key }}</th>
                <td>{{ row.value }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <p v-if="!hasExtended" class="wb-detail-empty">{{ t("detailNoMetadata") }}</p>
      </div>
      <footer>
        <button type="button" class="wb-dialog-primary wb-detail-close" @click="emit('close')">{{ t("close") }}</button>
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
</style>
