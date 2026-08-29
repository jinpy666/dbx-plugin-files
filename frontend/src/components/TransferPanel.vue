<script setup lang="ts">
import { computed } from "vue";
import { formatBytes, formatTime } from "../lib/api";
import { isByteBased, percentOf, type TransferJob } from "../lib/transfers";

const props = defineProps<{
  jobs: TransferJob[];
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "cancel", jobId: string): void;
}>();

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
  return `${formatBytes(job.transferred)} / ${formatBytes(job.size)}`;
}

function timeLabel(job: TransferJob): string {
  return formatTime(new Date(job.updatedAt).toISOString());
}
</script>

<template>
  <div v-if="active.length">
    <div class="wb-muted" style="margin: 2px 0 6px">{{ t("active") }}</div>
    <div v-for="job in active" :key="job.jobId" class="wb-transfer-item">
      <div class="wb-transfer-title">
        <strong :title="label(job)">{{ label(job) }}</strong>
        <span class="wb-transfer-state" :class="`is-${job.state}`">{{ stateLabel(job) }}</span>
        <button class="wb-icon-button wb-icon-danger" :title="t('cancelTransfer')" @click="emit('cancel', job.jobId)">✕</button>
      </div>
      <div class="wb-transfer-meta">
        <span>{{ t(`transferKind.${job.kind}`) }}</span>
        <span>{{ progressMeta(job) }} · {{ percentOf(job) }}%</span>
      </div>
      <div class="wb-progress"><div class="wb-progress-bar" :class="`is-${job.state}`" :style="{ width: `${percentOf(job)}%` }" /></div>
      <div v-if="job.error" class="wb-transfer-error">{{ job.error }}</div>
    </div>
  </div>
  <div v-if="history.length">
    <div class="wb-muted" style="margin: 8px 0 6px">{{ t("history") }}</div>
    <div v-for="job in history" :key="job.jobId" class="wb-transfer-item">
      <div class="wb-transfer-title">
        <strong :title="label(job)">{{ label(job) }}</strong>
        <span class="wb-transfer-state" :class="`is-${job.state}`">{{ stateLabel(job) }}</span>
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
