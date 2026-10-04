<script setup lang="ts">
import { computed, ref } from "vue";
import { CalendarClock, CircleCheck, CircleDashed, CircleX, LoaderCircle, Pencil, Play, Plus, Trash2 } from "@lucide/vue";
import { formatBytes, formatTime } from "../lib/api";
import { describeCron, isTaskRunning, lastFinishedRun, type RunStatus, type ScheduleRun, type ScheduleTask } from "../lib/schedules";

const props = defineProps<{
  tasks: ScheduleTask[];
  runs: ScheduleRun[];
  locale: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "create"): void;
  (event: "edit", task: ScheduleTask): void;
  (event: "delete", task: ScheduleTask): void;
  (event: "run-now", task: ScheduleTask): void;
  (event: "cancel-run", task: ScheduleTask): void;
  (event: "toggle", task: ScheduleTask, enabled: boolean): void;
}>();

/** 展开运行历史的任务 id（单开，避免长列表里多段历史挤压高度）。 */
const expandedId = ref<string | null>(null);

function toggleHistory(task: ScheduleTask): void {
  expandedId.value = expandedId.value === task.id ? null : task.id;
}

const orderedTasks = computed(() =>
  [...props.tasks].sort((a, b) => a.name.localeCompare(b.name)),
);

function cronLabel(task: ScheduleTask): string {
  return describeCron(task.cron, props.locale, props.t);
}

function kindLabel(task: ScheduleTask): string {
  return props.t(`scheduleKind.${task.kind}`);
}

function running(task: ScheduleTask): boolean {
  return isTaskRunning(props.runs, task.id);
}

function lastRun(task: ScheduleTask): ScheduleRun | undefined {
  return lastFinishedRun(props.runs, task.id);
}

function statusIconClass(status: RunStatus): string {
  return `is-${status}`;
}

const statusIcons = {
  running: LoaderCircle,
  success: CircleCheck,
  failed: CircleX,
  canceled: CircleX,
  skipped: CircleDashed,
} as const;

function nextRunLabel(task: ScheduleTask): string {
  if (!task.enabled) return props.t("scheduleDisabledHint");
  if (!task.nextRunAt) return "—";
  return formatTime(new Date(task.nextRunAt).toISOString());
}

function lastRunLabel(task: ScheduleTask): string {
  const run = lastRun(task);
  if (!run) return "—";
  return props.t(`scheduleRunStatus.${run.status}`);
}

/** 有告警的成功运行（verify/retention 注记）也要可读。 */
function isWarnedSuccess(task: ScheduleTask): boolean {
  const run = lastRun(task);
  return run?.status === "success" && !!run.error;
}

function taskRuns(task: ScheduleTask): ScheduleRun[] {
  return props.runs.filter((run) => run.taskId === task.id).slice(0, 10);
}

function runMeta(run: ScheduleRun): string {
  const parts = [
    props.t(run.trigger === "manual" ? "scheduleTriggerManual" : "scheduleTriggerSchedule"),
    formatTime(new Date(run.startedAt ?? run.finishedAt ?? 0).toISOString()),
  ];
  if (run.status === "success" || run.status === "failed") {
    parts.push(formatBytes(run.bytes));
    if (run.files !== undefined && run.files !== null) parts.push(`${run.files}`);
  }
  return parts.filter(Boolean).join(" · ");
}
</script>

<template>
  <div class="wb-schedules">
    <div class="wb-schedules-head">
      <span class="wb-muted">{{ t("scheduleIntro") }}</span>
      <button class="wb-icon-button" v-tip="t('scheduleNew')" @click="emit('create')"><Plus /></button>
    </div>

    <div v-for="task in orderedTasks" :key="task.id" class="wb-transfer-item">
      <div class="wb-transfer-title">
        <CalendarClock class="wb-transfer-status-icon" aria-hidden="true" />
        <strong>{{ task.name }}</strong>
        <!-- 启停开关：disabled 任务不触发，手动运行仍可用。 -->
        <label class="wb-schedule-toggle" v-tip="task.enabled ? t('scheduleEnabled') : t('scheduleDisabled')">
          <input
            type="checkbox"
            role="switch"
            :checked="task.enabled"
            :aria-label="`${task.name}: ${t('scheduleEnabled')}`"
            @change="emit('toggle', task, ($event.target as HTMLInputElement).checked)"
          />
        </label>
        <button v-if="!running(task)" class="wb-icon-button" v-tip="t('scheduleRunNow')" @click="emit('run-now', task)"><Play /></button>
        <button v-else class="wb-icon-button wb-icon-danger" v-tip="t('scheduleCancelRun')" @click="emit('cancel-run', task)"><CircleX /></button>
        <button class="wb-icon-button" v-tip="t('scheduleEdit')" @click="emit('edit', task)"><Pencil /></button>
        <button class="wb-icon-button wb-icon-danger" v-tip="t('scheduleDelete')" @click="emit('delete', task)"><Trash2 /></button>
      </div>
      <div class="wb-transfer-meta">
        <span>{{ kindLabel(task) }} · {{ cronLabel(task) }}</span>
        <button type="button" class="wb-schedule-expander" @click="toggleHistory(task)">
          {{ expandedId === task.id ? t("scheduleHideHistory") : t("scheduleShowHistory") }}
        </button>
      </div>
      <div class="wb-transfer-meta">
        <span v-tip="t('scheduleNextRun')">{{ t("scheduleNextRun") }}: {{ nextRunLabel(task) }}</span>
        <span
          class="wb-schedule-status"
          :class="running(task) ? 'is-running' : (lastRun(task)?.status === 'success' ? (isWarnedSuccess(task) ? 'is-warn' : 'is-ok') : `is-${lastRun(task)?.status ?? 'none'}`)"
        >{{ running(task) ? t("scheduleRunStatus.running") : lastRunLabel(task) }}</span>
      </div>
      <div v-if="task.options.backupDir" class="wb-transfer-meta">
        <span class="wb-mono wb-transfer-path-parent" v-tip="task.options.backupDir">{{ t("scheduleBackupDir") }}: {{ task.options.backupDir }}</span>
        <span v-if="task.options.retentionDays">{{ t("scheduleRetention", { days: task.options.retentionDays }) }}</span>
      </div>
      <div v-if="task.sourcePath !== task.targetPath || task.sourceConnectionId !== task.targetConnectionId" class="wb-transfer-meta">
        <span class="wb-mono wb-transfer-path-parent" v-tip="`${task.sourcePath} → ${task.targetPath}`">
          {{ task.sourcePath }} → {{ task.targetPath }}
        </span>
      </div>

      <div v-if="expandedId === task.id" class="wb-schedule-runs">
        <div v-if="!taskRuns(task).length" class="wb-muted" style="padding: 2px 0">{{ t("scheduleNoRuns") }}</div>
        <div v-for="run in taskRuns(task)" :key="run.runId" class="wb-schedule-run">
          <component
            :is="statusIcons[run.status]"
            class="wb-transfer-status-icon"
            :class="[`is-${run.status}`, { 'wb-spin': run.status === 'running' }]"
            role="img"
            v-tip="t(`scheduleRunStatus.${run.status}`)"
          />
          <span class="wb-schedule-run-meta">{{ runMeta(run) }}</span>
          <span v-if="run.status === 'running'" class="wb-schedule-status is-running">{{ t("scheduleRunStatus.running") }}</span>
          <span v-else-if="run.error" class="wb-transfer-error">{{ run.error }}</span>
        </div>
      </div>
    </div>

    <div v-if="!tasks.length" class="wb-file-empty wb-transfer-empty">
      <CalendarClock aria-hidden="true" />
      <strong>{{ t("scheduleEmptyTitle") }}</strong>
      <p>{{ t("scheduleEmptyHint") }}</p>
      <button class="wb-schedule-create" @click="emit('create')">{{ t("scheduleNew") }}</button>
    </div>
  </div>
</template>
