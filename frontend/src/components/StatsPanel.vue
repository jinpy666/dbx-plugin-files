<script setup lang="ts">
// 统计页签（对标 yet-another-rclone-dashboard 的 overview）：吞吐曲线 + 任务/
// 空间/限速汇总卡片。数据全部来自既有前端状态（transfer tracker 的 rateBps、
// files/about、files/bwlimit），不新增 sidecar 命令；吞吐历史为本地 1s 采样、
// 5 分钟滑动窗口，仅在本页签挂载期间采样（切走即停，不耗后台）。
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { formatBytes } from "../lib/api";
import { formatRate, isActive, isByteBased, type TransferJob } from "../lib/transfers";

const props = defineProps<{
  jobs: TransferJob[];
  /** 活动栏连接空间占用（files/about）；null = 本地连接或后端不支持。 */
  usage: { used: number; total: number } | null;
  /** 当前生效限速（files/bwlimit；null/空 = 不限速）。 */
  bwlimit?: string | null;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const THROUGHPUT_WINDOW_MS = 5 * 60 * 1000;
const SAMPLE_INTERVAL_MS = 1000;

const runningJobs = computed(() => props.jobs.filter((job) => job.state === "running"));
const currentRate = computed(() => runningJobs.value.reduce((sum, job) => sum + (job.rateBps ?? 0), 0));
const activeCount = computed(() => props.jobs.filter((job) => isActive(job.state)).length);
const completedCount = computed(() => props.jobs.filter((job) => job.state === "completed").length);
const failedCount = computed(() => props.jobs.filter((job) => job.state === "failed" || job.state === "canceled").length);
/** 历史任务累计字节数（目录 job 按 bytesDone，其余按 transferred）。 */
const totalBytes = computed(() =>
  props.jobs.filter((job) => !isActive(job.state)).reduce((sum, job) => sum + (isByteBased(job) ? job.bytesDone ?? 0 : job.transferred), 0),
);
const storagePercent = computed(() => (props.usage && props.usage.total ? Math.min(100, Math.round((props.usage.used / props.usage.total) * 100)) : 0));

interface SpeedSample {
  at: number;
  value: number;
}
const samples = ref<SpeedSample[]>([]);
let sampleTimer: ReturnType<typeof setInterval> | undefined;

function pushSample() {
  const now = Date.now();
  const windowStart = now - THROUGHPUT_WINDOW_MS;
  const next = samples.value.filter((sample) => sample.at >= windowStart);
  next.push({ at: now, value: currentRate.value });
  samples.value = next;
}

onMounted(() => {
  pushSample();
  sampleTimer = setInterval(pushSample, SAMPLE_INTERVAL_MS);
});
onBeforeUnmount(() => {
  if (sampleTimer) clearInterval(sampleTimer);
});

const CHART_TOP = 8;
const CHART_BOTTOM = 88;

// viewBox 0..100 归一化（preserveAspectRatio=none 拉伸铺满）；依赖 samples 每
// 秒更新而重算，「now」取最新采样时刻，避免引入非响应式时钟。
const chart = computed(() => {
  const list = samples.value;
  if (list.length < 2) return null;
  const now = list[list.length - 1]!.at;
  const windowStart = now - THROUGHPUT_WINDOW_MS;
  const visible = list.filter((sample) => sample.at >= windowStart);
  if (visible.length < 2) return null;
  const maxValue = Math.max(...visible.map((sample) => sample.value), 1);
  const x = (at: number) => ((at - windowStart) / THROUGHPUT_WINDOW_MS) * 100;
  const y = (value: number) => CHART_BOTTOM - (Math.max(0, value) / maxValue) * (CHART_BOTTOM - CHART_TOP);
  const line = visible.map((sample, index) => `${index === 0 ? "M" : "L"} ${x(sample.at).toFixed(2)},${y(sample.value).toFixed(2)}`).join(" ");
  const first = visible[0]!;
  const last = visible[visible.length - 1]!;
  const area = `${line} L ${x(last.at).toFixed(2)},${CHART_BOTTOM} L ${x(first.at).toFixed(2)},${CHART_BOTTOM} Z`;
  return { line, area, peak: maxValue };
});
</script>

<template>
  <div class="wb-stats">
    <div class="wb-stats-card">
      <div class="wb-stats-card-head">
        <span class="wb-muted">{{ t("statsThroughput") }}</span>
        <strong class="wb-stats-rate" :class="{ 'is-idle': !currentRate }">{{ formatRate(currentRate) }}</strong>
      </div>
      <svg v-if="chart" class="wb-stats-chart" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
        <line
          v-for="i in 6"
          :key="i"
          x1="0"
          x2="100"
          :y1="CHART_TOP + ((CHART_BOTTOM - CHART_TOP) * (i - 1)) / 5"
          :y2="CHART_TOP + ((CHART_BOTTOM - CHART_TOP) * (i - 1)) / 5"
          class="wb-stats-grid"
        />
        <path :d="chart.area" class="wb-stats-area" />
        <path :d="chart.line" class="wb-stats-line" />
      </svg>
      <div v-else class="wb-stats-chart-empty wb-muted">{{ t("statsChartEmpty") }}</div>
      <div class="wb-stats-chart-axis"><span>-5m</span><span>now</span></div>
    </div>
    <div class="wb-stats-cards">
      <div class="wb-stats-cell">
        <span class="wb-muted">{{ t("statsActiveTransfers") }}</span>
        <strong>{{ activeCount }}</strong>
      </div>
      <div class="wb-stats-cell">
        <span class="wb-muted">{{ t("statsCompleted") }}</span>
        <strong>{{ completedCount }}</strong>
      </div>
      <div class="wb-stats-cell" :class="{ 'is-bad': failedCount > 0 }">
        <span class="wb-muted">{{ t("statsFailed") }}</span>
        <strong>{{ failedCount }}</strong>
      </div>
      <div class="wb-stats-cell">
        <span class="wb-muted">{{ t("statsTotalBytes") }}</span>
        <strong>{{ formatBytes(totalBytes) }}</strong>
      </div>
      <div class="wb-stats-cell">
        <span class="wb-muted">{{ t("bwlimitLabel") }}</span>
        <strong class="wb-stats-limit" :class="{ 'is-on': Boolean(bwlimit) }">{{ bwlimit || t("bwlimitUnlimited") }}</strong>
      </div>
    </div>
    <div class="wb-stats-card">
      <div class="wb-stats-card-head">
        <span class="wb-muted">{{ t("statsStorage") }}</span>
        <strong v-if="usage && usage.total > 0">{{ storagePercent }}%</strong>
      </div>
      <template v-if="usage && usage.total > 0">
        <div class="wb-progress" role="progressbar" :aria-label="t('statsStorage')" aria-valuemin="0" aria-valuemax="100" :aria-valuenow="storagePercent">
          <div class="wb-progress-bar is-running" :style="{ width: `${storagePercent}%` }" />
        </div>
        <div class="wb-muted wb-stats-storage-meta">{{ formatBytes(usage.used) }} / {{ formatBytes(usage.total) }}</div>
      </template>
      <div v-else class="wb-muted wb-stats-storage-meta">{{ t("statsStorageUnavailable") }}</div>
    </div>
  </div>
</template>
