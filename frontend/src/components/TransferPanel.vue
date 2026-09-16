<script setup lang="ts">
import { computed } from "vue";
import { RotateCw, Trash2, X } from "@lucide/vue";
import { formatBytes, formatTime } from "../lib/api";
import { etaSeconds, formatEta, formatRate, isByteBased, isRetryableKind, percentOf, type TransferJob } from "../lib/transfers";

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
}>();

const retryable = computed(() => new Set(props.retryableIds ?? []));

/** 失败且可原样重发的历史任务才显示 ↻（kind 可重发 + App 有登记参数）。 */
function canRetry(job: TransferJob): boolean {
  return job.state === "failed" && isRetryableKind(job.kind) && retryable.value.has(job.jobId);
}

const active = computed(() => props.jobs.filter((job) => job.state === "queued" || job.state === "running"));
const history = computed(() => props.jobs.filter((job) => job.state !== "queued" && job.state !== "running"));

function label(job: TransferJob): string {
  return job.remotePath || job.jobId;
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
  // issue#6-1b：历史时间优先用 sidecar 完成时刻（finishedAt，轮询不覆盖），
  // 退回事件流写入的 updatedAt——两者都不随本机时钟漂移。
  return formatTime(new Date(job.finishedAt ?? job.updatedAt).toISOString());
}
</script>

<template>
  <div v-if="active.length">
    <div class="wb-muted" style="margin: 2px 0 6px">{{ t("active") }}</div>
    <div v-for="job in active" :key="job.jobId" class="wb-transfer-item">
      <div class="wb-transfer-title">
        <strong :title="label(job)">{{ label(job) }}</strong>
        <span class="wb-transfer-state" :class="`is-${job.state}`">{{ stateLabel(job) }}</span>
        <button class="wb-icon-button wb-icon-danger" v-tip="t('cancelTransfer')" @click="emit('cancel', job.jobId)"><X /></button>
      </div>
      <div class="wb-transfer-meta">
        <span>{{ t(`transferKind.${job.kind}`) }}</span>
        <span>{{ progressMeta(job) }} · {{ percentOf(job) }}%</span>
      </div>
      <div v-if="speedMeta(job)" class="wb-transfer-meta"><span class="wb-transfer-speed">{{ speedMeta(job) }}</span></div>
      <div class="wb-progress"><div class="wb-progress-bar" :class="`is-${job.state}`" :style="{ width: `${percentOf(job)}%` }" /></div>
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
        <strong :title="label(job)">{{ label(job) }}</strong>
        <span class="wb-transfer-state" :class="`is-${job.state}`">{{ stateLabel(job) }}</span>
        <button v-if="canRetry(job)" class="wb-icon-button" v-tip="t('retryTransfer')" @click="emit('retry', job.jobId)"><RotateCw /></button>
      </div>
      <div class="wb-transfer-meta">
        <span>{{ t(`transferKind.${job.kind}`) }} · {{ timeLabel(job) }}</span>
        <span>{{ progressMeta(job) }}</span>
      </div>
      <div v-if="job.error" class="wb-transfer-error">{{ job.error }}</div>
    </div>
  </div>
  <div v-if="!jobs.length" class="wb-file-empty">{{ t("noTransfers") }}</div>
</template>
