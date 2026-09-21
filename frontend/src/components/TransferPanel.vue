<script setup lang="ts">
import { computed } from "vue";
import { CircleCheck, CircleDashed, CircleX, ExternalLink, FolderOpen, FileText, LoaderCircle, RotateCw, Trash2, X } from "@lucide/vue";
import { formatBytes, formatTime } from "../lib/api";
import { etaSeconds, formatEta, formatRate, isByteBased, isRetryableKind, percentOf, sortedJobs, splitTransferPath, transferPathLabel, type TransferJob } from "../lib/transfers";

/** 复制差异摘要（审计友好的纯文本）；剪贴板不可用时静默失败。 */
async function copyCheckSummary(job: { checkSummary?: string; jobId: string }) {
  const text = job.checkSummary;
  if (!text) return;
  try {
    await window.dbxPlugin?.clipboard?.writeText(text);
  } catch {
    // 无剪贴板能力（旧宿主/web）时忽略。
  }
}

const props = defineProps<{
  jobs: TransferJob[];
  t: (key: string, values?: Record<string, string | number>) => string;
  /** 提交时登记了原始请求参数的失败 job（App 侧权威判定）。 */
  retryableIds?: string[];
}>();

const emit = defineEmits<{
  (event: "cancel", jobId: string): void;
  (event: "clear-history"): void;
  (event: "retry", jobId: string): void;
  (event: "delete", jobId: string): void;
  (event: "reveal", path: string): void;
  (event: "open", path: string): void;
  /** issue #11：用用户配置的外部应用打开（App 侧按偏好解析出 app 再下发）。 */
  (event: "open-app", path: string): void;
}>();

const retryable = computed(() => new Set(props.retryableIds ?? []));

const statusIcons = {
  queued: CircleDashed,
  running: LoaderCircle,
  completed: CircleCheck,
  failed: CircleX,
  canceled: CircleX,
} as const;

function statusIcon(job: TransferJob) {
  return statusIcons[job.state];
}

function statusLabel(job: TransferJob): string {
  return stateLabel(job);
}

/** 失败且可原样重发的历史任务才显示 ↻（kind 可重发 + App 有登记参数）。 */
function canRetry(job: TransferJob): boolean {
  return job.state === "failed" && isRetryableKind(job.kind) && retryable.value.has(job.jobId);
}

/** 已完成的本机下载才可定位/打开（sidecar 按历史白名单二次校验）。 */
function canReveal(job: TransferJob): boolean {
  return !!job.localPath;
}

const orderedJobs = computed(() => {
  const jobs = Object.fromEntries(props.jobs.map((job) => [job.jobId, job]));
  return sortedJobs(jobs);
});
const active = computed(() => orderedJobs.value.filter((job) => job.state === "queued" || job.state === "running"));
const history = computed(() => orderedJobs.value.filter((job) => job.state !== "queued" && job.state !== "running"));

function label(job: TransferJob): string {
  return transferPathLabel(job.remotePath || job.jobId);
}

function fullLabel(job: TransferJob): string {
  return job.remotePath || job.jobId;
}

function pathParts(path: string) {
  return splitTransferPath(path);
}

function stateLabel(job: TransferJob): string {
  return props.t(`transferStatus.${job.state}`);
}

function progressMeta(job: TransferJob): string {
  if (isByteBased(job)) {
    const files = job.filesTotal ? props.t("filesProgress", { done: job.filesDone ?? 0, total: job.filesTotal }) : "";
    const bytes = job.bytesTotal ? `${formatBytes(job.bytesDone ?? 0)} / ${formatBytes(job.bytesTotal)}` : "";
    return [files, bytes].filter(Boolean).join(" · ") || `${formatBytes(job.transferred)} / ${formatBytes(job.size)}`;
  }
  // R5-P2-3：非字节型但带 filesTotal 的计数型 job（批量删除伪 job 的
  // transferred/size 实为文件个数）按「N/M 项」渲染，不得误格式化成字节
  // （此前显示「删除 504 B / 9.8 KiB」这类计数+字节混排版）。
  if (job.filesTotal) return props.t("filesProgress", { done: job.filesDone ?? 0, total: job.filesTotal });
  return `${formatBytes(job.transferred)} / ${formatBytes(job.size)}`;
}

/** 运行中且速率已知 → `「2.3 MiB/s · 剩余 ~45s` 形态的速率段（语言无关）。 */
function speedMeta(job: TransferJob): string {
  if (job.state !== "running" || !job.rateBps) return "";
  const eta = etaSeconds(job);
  return [formatRate(job.rateBps), eta !== undefined ? `~${formatEta(eta)}` : ""].filter(Boolean).join(" · ");
}

function timeLabel(job: TransferJob): string {
  // 历史时间显示任务加入/登记时刻，而不是完成时刻。
  return formatTime(new Date(job.createdAt ?? job.startedAt ?? job.updatedAt).toISOString());
}
</script>

<template>
  <div v-if="active.length">
    <div class="wb-muted" style="margin: 2px 0 6px">{{ t("active") }}</div>
    <div v-for="job in active" :key="job.jobId" class="wb-transfer-item">
      <div class="wb-transfer-title">
        <component
          :is="statusIcon(job)"
          class="wb-transfer-status-icon"
          :class="[`is-${job.state}`, { 'wb-spin': job.state === 'running' }]"
          :title="statusLabel(job)"
          :aria-label="statusLabel(job)"
          role="img"
        />
        <strong :title="fullLabel(job)">{{ label(job) }}</strong>
        <button class="wb-icon-button wb-icon-danger" v-tip="t('cancelTransfer')" @click="emit('cancel', job.jobId)"><X /></button>
      </div>
      <div v-if="pathParts(job.remotePath || job.jobId).parent" class="wb-transfer-localpath wb-mono" :title="fullLabel(job)">
        <span class="wb-transfer-path-parent">{{ pathParts(job.remotePath || job.jobId).parent }}</span>
      </div>
      <div class="wb-transfer-meta">
        <span>{{ t(`transferKind.${job.kind}`) }}</span>
        <button
          v-if="job.checkSummary"
          type="button"
          class="wb-check-summary"
          :class="job.checkSummary.startsWith('identical') ? 'is-ok' : 'is-diff'"
          v-tip="t('checkSummaryCopy')"
          @click="copyCheckSummary(job)"
        >{{ job.checkSummary }}</button>
        <span>{{ progressMeta(job) }} · {{ percentOf(job) }}%</span>
      </div>
      <div v-if="speedMeta(job)" class="wb-transfer-meta"><span class="wb-transfer-speed">{{ speedMeta(job) }}</span></div>
      <!-- 审计中#11：进度条从纯视觉升级为 progressbar 语义，值变化可被读屏感知。 -->
      <div
        class="wb-progress"
        role="progressbar"
        :aria-label="`${t(`transferKind.${job.kind}`)}: ${label(job)}`"
        aria-valuemin="0"
        aria-valuemax="100"
        :aria-valuenow="percentOf(job)"
      ><div class="wb-progress-bar" :class="`is-${job.state}`" :style="{ width: `${percentOf(job)}%` }" /></div>
      <div v-if="job.error" class="wb-transfer-error">{{ job.error }}</div>
    </div>
  </div>
  <div v-if="history.length">
    <div class="wb-transfer-history-head" style="margin: 8px 0 6px">
      <span class="wb-muted">{{ t("history") }}</span>
      <button class="wb-icon-button" v-tip="t('clearHistory')" @click="emit('clear-history')"><Trash2 /></button>
    </div>
    <div v-for="job in history" :key="job.jobId" class="wb-transfer-item">
      <div class="wb-transfer-title">
        <component
          :is="statusIcon(job)"
          class="wb-transfer-status-icon"
          :class="[`is-${job.state}`, { 'wb-spin': job.state === 'running' }]"
          :title="statusLabel(job)"
          :aria-label="statusLabel(job)"
          role="img"
        />
        <strong :title="fullLabel(job)">{{ label(job) }}</strong>
        <button v-if="canRetry(job)" class="wb-icon-button" v-tip="t('retryTransfer')" @click="emit('retry', job.jobId)"><RotateCw /></button>
        <button v-if="canReveal(job)" class="wb-icon-button" v-tip="t('revealInFolder')" @click="emit('reveal', job.localPath!)"><FolderOpen /></button>
        <button v-if="canReveal(job)" class="wb-icon-button" v-tip="t('openDownloadedFile')" @click="emit('open', job.localPath!)"><FileText /></button>
        <!-- issue #11：外部应用打开（未配置偏好时 App 侧提示去设置页）。 -->
        <button v-if="canReveal(job)" class="wb-icon-button" v-tip="t('openWithExternalApp')" @click="emit('open-app', job.localPath!)"><ExternalLink /></button>
        <button class="wb-icon-button wb-icon-danger" v-tip="t('deleteRecord')" @click="emit('delete', job.jobId)"><Trash2 /></button>
      </div>
      <div class="wb-transfer-meta">
        <span>{{ t(`transferKind.${job.kind}`) }} · {{ timeLabel(job) }}</span>
        <span>{{ progressMeta(job) }}</span>
      </div>
      <div v-if="job.checkSummary" class="wb-transfer-meta">
        <button
          type="button"
          class="wb-check-summary"
          :class="job.checkSummary.startsWith('identical') ? 'is-ok' : 'is-diff'"
          v-tip="t('checkSummaryCopy')"
          @click="copyCheckSummary(job)"
        >{{ job.checkSummary }}</button>
      </div>
      <p v-if="job.localPath" class="wb-transfer-localpath wb-mono" :title="job.localPath">
        <span v-if="pathParts(job.localPath!).parent" class="wb-transfer-path-parent">{{ pathParts(job.localPath!).parent }}</span>
        <span class="wb-transfer-path-name">{{ pathParts(job.localPath!).name }}</span>
      </p>
      <div v-if="job.error" class="wb-transfer-error">{{ job.error }}</div>
    </div>
  </div>
  <div v-if="!jobs.length" class="wb-file-empty">{{ t("noTransfers") }}</div>
</template>
