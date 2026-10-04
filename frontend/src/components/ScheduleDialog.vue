<script setup lang="ts">
// 计划任务创建/编辑对话框（MountDialog/SyncDialog 同形态顶层弹窗）。
// 任务 = 连接对 + 路径对 + 五字段 cron + rclone 选项（backupDir 版本化、
// 保留策略、运行后校验、过滤器）。cron 在此做即时校验与下次运行预览，
// 后端 create/update 仍是唯一权威校验。kind=bisync 时两侧都必须可写可删，
// 由后端拒绝；这里只在 UI 上给出提示。
import { computed, ref, watch } from "vue";
import { FolderOpen, X } from "@lucide/vue";
import { formatTime } from "../lib/api";
import { trapTabKey } from "../lib/a11y";
import {
  CRON_PRESETS,
  describeCron,
  isValidCron,
  nextRunAfter,
  presetIdOf,
  type ScheduleKind,
  type ScheduleOptions,
  type ScheduleTask,
} from "../lib/schedules";
import DirectoryBrowser from "./DirectoryBrowser.vue";

export interface ScheduleConnectionOption {
  id: string;
  label: string;
}

export interface ScheduleDraft {
  id?: string;
  name: string;
  kind: ScheduleKind;
  sourceConnectionId: string;
  sourcePath: string;
  targetConnectionId: string;
  targetPath: string;
  cron: string;
  enabled: boolean;
  options: ScheduleOptions;
}

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  locale: string;
  /** null = 新建；否则编辑该任务。 */
  task: ScheduleTask | null;
  /** 可选连接（App 侧从宿主连接列表映射）。 */
  connections: ScheduleConnectionOption[];
  busy?: boolean;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "confirm", draft: ScheduleDraft): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

const name = ref(props.task?.name ?? "");
const kind = ref<ScheduleKind>(props.task?.kind ?? "sync");
const sourceConnectionId = ref(props.task?.sourceConnectionId ?? props.connections[0]?.id ?? "");
const sourcePath = ref(props.task?.sourcePath ?? "/");
const targetConnectionId = ref(props.task?.targetConnectionId ?? props.task?.sourceConnectionId ?? props.connections[0]?.id ?? "");
const targetPath = ref(props.task?.targetPath ?? "");
const cron = ref(props.task?.cron ?? CRON_PRESETS[1].cron);
const enabled = ref(props.task?.enabled ?? true);
// 切换源连接时目标跟随（最常见是同连接备份）；用户显式改过目标后不再跟。
const targetTouched = ref(Boolean(props.task && props.task.targetConnectionId !== props.task.sourceConnectionId));
const backupDir = ref(props.task?.options.backupDir ?? "");
const suffix = ref(props.task?.options.suffix ?? "");
const retentionDays = ref<number | null>(props.task?.options.retentionDays ?? null);
const verifyAfter = ref(props.task?.options.verifyAfter ?? false);
const include = ref((props.task?.options.include ?? []).join(", "));
const exclude = ref((props.task?.options.exclude ?? []).join(", "));
const maxDelete = ref<number | null>(props.task?.options.maxDelete ?? null);
const dryRun = ref(props.task?.options.dryRun ?? false);

watch(sourceConnectionId, (id) => {
  if (!targetTouched.value) targetConnectionId.value = id;
});

const kindHints = computed(() => ({
  sync: t("scheduleKindHintSync"),
  copy: t("scheduleKindHintCopy"),
  bisync: t("scheduleKindHintBisync"),
}));

const cronValid = computed(() => isValidCron(cron.value));
const cronDescription = computed(() => describeCron(cron.value, props.locale, t));
const nextRunPreview = computed(() => {
  if (!cronValid.value) return null;
  const next = nextRunAfter(cron.value);
  return next ? formatTime(next.toISOString()) : null;
});
const sameTarget = computed(
  () => kind.value !== "bisync" && sourceConnectionId.value === targetConnectionId.value && sourcePath.value.trim() === targetPath.value.trim(),
);
const canConfirm = computed(
  () =>
    !props.busy &&
    name.value.trim().length > 0 &&
    cronValid.value &&
    sourcePath.value.trim().length > 0 &&
    targetPath.value.trim().length > 0 &&
    !sameTarget.value,
);

// 路径浏览：source/target 二选一展开，跟随当前编辑的连接。
const browseField = ref<"source" | "target" | null>(null);
const browserStart = ref("/");
function toggleBrowse(field: "source" | "target"): void {
  if (browseField.value === field) {
    browseField.value = null;
    return;
  }
  browseField.value = field;
  const path = field === "source" ? sourcePath.value : targetPath.value;
  browserStart.value = path || "/";
}
function onBrowseNavigate(path: string): void {
  if (browseField.value === "source") sourcePath.value = path;
  else if (browseField.value === "target") targetPath.value = path;
}

function confirm(): void {
  if (!canConfirm.value) return;
  const patterns = (text: string): string[] | undefined => {
    const list = text.split(",").map((entry) => entry.trim()).filter(Boolean);
    return list.length ? list : undefined;
  };
  const options: ScheduleOptions = {
    dryRun: dryRun.value || undefined,
    maxDelete: maxDelete.value ?? undefined,
    include: patterns(include.value),
    exclude: patterns(exclude.value),
    backupDir: backupDir.value.trim() || undefined,
    suffix: suffix.value.trim() || undefined,
    verifyAfter: verifyAfter.value,
    retentionDays: retentionDays.value ?? undefined,
  };
  emit("confirm", {
    id: props.task?.id,
    name: name.value.trim(),
    kind: kind.value,
    sourceConnectionId: sourceConnectionId.value,
    sourcePath: sourcePath.value,
    targetConnectionId: targetConnectionId.value,
    targetPath: targetPath.value,
    cron: cron.value.trim(),
    enabled: enabled.value,
    options,
  });
}

const dialogEl = ref<HTMLElement | null>(null);
function onTabKeydown(event: KeyboardEvent): void {
  trapTabKey(event, dialogEl.value);
}
</script>

<template>
  <div class="wb-mount-backdrop" role="dialog" aria-modal="true" :aria-label="task ? t('scheduleEdit') : t('scheduleNew')" @click.self="!busy && emit('close')">
    <div ref="dialogEl" class="wb-mount-dialog wb-sync-dialog" @keydown="onTabKeydown">
      <header>
        <strong>{{ task ? t("scheduleEditTitle", { name: task.name }) : t("scheduleNewTitle") }}</strong>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" :disabled="busy" @click="emit('close')"><X /></button>
      </header>
      <p class="wb-mount-hint">{{ t("scheduleDialogHint") }}</p>

      <label class="wb-mount-field">
        <span>{{ t("scheduleNameLabel") }}</span>
        <input v-model="name" spellcheck="false" :placeholder="t('scheduleNamePlaceholder')" :aria-label="t('scheduleNameLabel')" />
      </label>

      <div class="wb-mount-field">
        <span>{{ t("scheduleKindLabel") }}</span>
        <div class="wb-schedule-kinds" role="radiogroup" :aria-label="t('scheduleKindLabel')">
          <label v-for="option in (['sync', 'copy', 'bisync'] as const)" :key="option" class="wb-sync-check">
            <input v-model="kind" type="radio" name="schedule-kind" :value="option" />
            <span>{{ t(`scheduleKind.${option}`) }}</span>
          </label>
        </div>
        <p class="wb-mount-hint">{{ kindHints[kind] }}</p>
      </div>

      <div class="wb-mount-field">
        <span>{{ t("syncSourceLabel") }}</span>
        <div class="wb-sync-path-row">
          <select v-model="sourceConnectionId" :aria-label="t('syncSourceLabel')">
            <option v-for="connection in connections" :key="connection.id" :value="connection.id">{{ connection.label }}</option>
          </select>
          <input v-model="sourcePath" class="wb-mono" spellcheck="false" :aria-label="t('syncSourceLabel')" />
          <button
            type="button"
            class="wb-icon-button wb-icon-neutral wb-sync-browse"
            :class="{ 'wb-sync-browse-active': browseField === 'source' }"
            :aria-label="t('syncBrowsePath')"
            v-tip="t('syncBrowsePath')"
            @click="toggleBrowse('source')"
          ><FolderOpen /></button>
        </div>
      </div>

      <div class="wb-mount-field">
        <span>{{ t("scheduleTargetLabel") }}</span>
        <div class="wb-sync-path-row">
          <select v-model="targetConnectionId" :aria-label="t('scheduleTargetLabel')" @change="targetTouched = true">
            <option v-for="connection in connections" :key="connection.id" :value="connection.id">{{ connection.label }}</option>
          </select>
          <input v-model="targetPath" class="wb-mono" spellcheck="false" :placeholder="t('scheduleTargetPlaceholder')" :aria-label="t('scheduleTargetLabel')" />
          <button
            type="button"
            class="wb-icon-button wb-icon-neutral wb-sync-browse"
            :class="{ 'wb-sync-browse-active': browseField === 'target' }"
            :aria-label="t('syncBrowsePath')"
            v-tip="t('syncBrowsePath')"
            @click="toggleBrowse('target')"
          ><FolderOpen /></button>
        </div>
      </div>
      <DirectoryBrowser
        v-if="browseField"
        :key="`${browseField}:${browserStart}:${browseField === 'source' ? sourceConnectionId : targetConnectionId}`"
        :t="t"
        :connection-id="browseField === 'source' ? sourceConnectionId : targetConnectionId"
        :initial-path="browserStart"
        missing-hint-key="syncBrowseMissing"
        @navigate="onBrowseNavigate"
      />

      <p v-if="sameTarget" class="wb-mount-hint wb-sync-warning" role="alert">{{ t("destMustDiffer") }}</p>

      <div class="wb-mount-field">
        <span>{{ t("scheduleCronLabel") }}</span>
        <div class="wb-sync-path-row">
          <select :value="presetIdOf(cron)" :aria-label="t('schedulePresetLabel')" @change="cron = (($event.target as HTMLSelectElement).value === 'custom' ? cron : (($event.target as HTMLSelectElement).selectedOptions[0]?.dataset.cron || cron))">
            <option v-for="preset in CRON_PRESETS" :key="preset.id" :value="preset.id" :data-cron="preset.cron">{{ t(`schedulePreset.${preset.id}`) }}</option>
            <option value="custom">{{ t("schedulePreset.custom") }}</option>
          </select>
          <input v-model="cron" class="wb-mono" spellcheck="false" placeholder="30 3 * * *" :aria-label="t('scheduleCronLabel')" />
        </div>
        <p v-if="!cronValid" class="wb-mount-hint wb-sync-warning" role="alert">{{ t("scheduleCronInvalid") }}</p>
        <template v-else>
          <p class="wb-mount-hint">{{ cronDescription }}</p>
          <p v-if="nextRunPreview" class="wb-mount-hint">{{ t("scheduleNextRunPreview", { time: nextRunPreview }) }}</p>
        </template>
      </div>

      <label class="wb-sync-check">
        <input v-model="enabled" type="checkbox" />
        <span>{{ t("scheduleEnabled") }}</span>
      </label>

      <details class="wb-schedule-advanced">
        <summary>{{ t("scheduleAdvancedLabel") }}</summary>
        <div class="wb-sync-grid">
          <label class="wb-mount-field">
            <span>{{ t("syncBackupDirLabel") }}</span>
            <input v-model="backupDir" class="wb-mono" spellcheck="false" :placeholder="t('scheduleBackupDirPlaceholder')" />
          </label>
          <label class="wb-mount-field">
            <span>{{ t("syncSuffixLabel") }}</span>
            <input v-model="suffix" class="wb-mono" spellcheck="false" placeholder=".bak" />
          </label>
          <label class="wb-mount-field">
            <span>{{ t("scheduleRetentionLabel") }}</span>
            <input v-model.number="retentionDays" type="number" min="1" step="1" :placeholder="t('scheduleRetentionPlaceholder')" />
          </label>
          <label class="wb-mount-field">
            <span>{{ t("syncMaxDeleteLabel") }}</span>
            <input v-model.number="maxDelete" type="number" min="0" step="1" :placeholder="t('syncMaxDeletePlaceholder')" />
          </label>
          <label class="wb-mount-field">
            <span>{{ t("syncIncludeLabel") }}</span>
            <input v-model="include" class="wb-mono" spellcheck="false" :placeholder="t('syncIncludePlaceholder')" />
          </label>
          <label class="wb-mount-field">
            <span>{{ t("syncExcludeLabel") }}</span>
            <input v-model="exclude" class="wb-mono" spellcheck="false" :placeholder="t('syncExcludePlaceholder')" />
          </label>
        </div>
        <label class="wb-sync-check">
          <input v-model="verifyAfter" type="checkbox" />
          <span>{{ t("scheduleVerifyLabel") }}</span>
        </label>
        <p v-if="verifyAfter" class="wb-mount-hint">{{ t("scheduleVerifyHint") }}</p>
        <label class="wb-sync-check">
          <input v-model="dryRun" type="checkbox" />
          <span>{{ t("syncDryRunLabel") }}</span>
        </label>
        <p v-if="backupDir.trim() && retentionDays" class="wb-mount-hint">{{ t("scheduleRetentionHint") }}</p>
        <p v-if="kind === 'sync' && !backupDir.trim()" class="wb-mount-hint">{{ t("scheduleBackupDirHint") }}</p>
      </details>

      <footer>
        <span class="wb-muted wb-mount-foot-hint">{{ t("scheduleRuntimeHint") }}</span>
        <span class="wb-mount-foot-actions">
          <button class="wb-dialog-cancel" type="button" :disabled="busy" @click="emit('close')">{{ t("cancel") }}</button>
          <button class="wb-dialog-primary" type="button" :disabled="!canConfirm" @click="confirm">{{ busy ? t("loading") : t("scheduleSave") }}</button>
        </span>
      </footer>
    </div>
  </div>
</template>
