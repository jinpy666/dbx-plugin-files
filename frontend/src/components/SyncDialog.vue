<script setup lang="ts">
// 目录同步/复制选项对话框（独立顶层弹窗，同 MountDialog 形态）：目标路径 +
// dry-run 预览 + include/exclude 过滤 + backup-dir/suffix 备份 + 高级并发参数。
// 确认时把非空字段打包成 SyncDialogOptions 交给父层发起 files/syncDir|copyDir；
// 数值字段在组件内先夹紧范围（rc 对非法值静默忽略，夹紧是唯一防线）。
import { ref } from "vue";
import { X } from "@lucide/vue";

/** 确认载荷：空串/空数组/null = 不传该字段（保持 rclone 默认）。 */
export interface SyncDialogOptions {
  targetPath: string;
  dryRun: boolean;
  include: string[];
  exclude: string[];
  backupDir: string;
  suffix: string;
  transfers: number | null;
  checkers: number | null;
  retries: number | null;
}

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  kind: "syncDir" | "copyDir";
  sourcePath: string;
  defaultTarget: string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "confirm", options: SyncDialogOptions): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

const targetPath = ref(props.defaultTarget);
const dryRun = ref(false);
const include = ref("");
const exclude = ref("");
const backupDir = ref("");
const suffix = ref("");
const advancedOpen = ref(false);
const transfers = ref("");
const checkers = ref("");
const retries = ref("");

/** 逗号/换行分隔 → 去空白的模式数组。 */
function parsePatterns(input: string): string[] {
  return input
    .split(/[,\n]/)
    .map((pattern) => pattern.trim())
    .filter(Boolean);
}

/** 整数夹紧（非法/越界输入折回边界；空串 = 未设置）。 */
function parseCount(input: string, min: number, max: number): number | null {
  const trimmed = input.trim();
  if (!trimmed) return null;
  const value = Number.parseInt(trimmed, 10);
  if (!Number.isFinite(value)) return null;
  return Math.min(max, Math.max(min, value));
}

function confirm() {
  if (!targetPath.value.trim()) return;
  emit("confirm", {
    targetPath: targetPath.value.trim(),
    dryRun: dryRun.value,
    include: parsePatterns(include.value),
    exclude: parsePatterns(exclude.value),
    backupDir: backupDir.value.trim(),
    suffix: suffix.value.trim(),
    transfers: parseCount(transfers.value, 1, 32),
    checkers: parseCount(checkers.value, 1, 64),
    retries: parseCount(retries.value, 1, 10),
  });
}
</script>

<template>
  <div class="wb-mount-backdrop" role="dialog" aria-modal="true" :aria-label="t('syncOptionsTitle')" @click.self="emit('close')">
    <div class="wb-mount-dialog wb-sync-dialog">
      <header>
        <strong>{{ kind === "syncDir" ? t("transferKind.syncDir") : t("transferKind.copyDir") }}</strong>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="emit('close')"><X /></button>
      </header>
      <p class="wb-mount-hint">{{ kind === "syncDir" ? t("syncDirBody") : t("copyDirBody") }}</p>

      <label class="wb-mount-field">
        <span>{{ t("syncSourceLabel") }}</span>
        <output class="wb-mono">{{ sourcePath }}</output>
      </label>
      <label class="wb-mount-field">
        <span>{{ t("syncTargetLabel") }}</span>
        <input v-model="targetPath" class="wb-mono" spellcheck="false" :placeholder="defaultTarget" />
      </label>

      <label class="wb-sync-check">
        <input v-model="dryRun" type="checkbox" />
        <span>{{ t("syncDryRunLabel") }}</span>
      </label>
      <p v-if="dryRun" class="wb-mount-hint">{{ t("syncDryRunHint") }}</p>

      <div class="wb-sync-grid">
        <label class="wb-mount-field">
          <span>{{ t("syncIncludeLabel") }}</span>
          <input v-model="include" class="wb-mono" spellcheck="false" :placeholder="t('syncIncludePlaceholder')" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncExcludeLabel") }}</span>
          <input v-model="exclude" class="wb-mono" spellcheck="false" :placeholder="t('syncExcludePlaceholder')" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncBackupDirLabel") }}</span>
          <input v-model="backupDir" class="wb-mono" spellcheck="false" :placeholder="t('syncBackupDirPlaceholder')" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncSuffixLabel") }}</span>
          <input v-model="suffix" class="wb-mono" spellcheck="false" placeholder=".bak" />
        </label>
      </div>
      <p class="wb-mount-hint">{{ t("syncBackupDirHint") }}</p>

      <button type="button" class="wb-sync-advanced-toggle" @click="advancedOpen = !advancedOpen">
        {{ advancedOpen ? t("syncAdvancedHide") : t("syncAdvancedShow") }}
      </button>
      <div v-show="advancedOpen" class="wb-sync-grid">
        <label class="wb-mount-field">
          <span>{{ t("syncTransfersLabel") }}</span>
          <input v-model="transfers" class="wb-mono" inputmode="numeric" placeholder="4" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncCheckersLabel") }}</span>
          <input v-model="checkers" class="wb-mono" inputmode="numeric" placeholder="8" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncRetriesLabel") }}</span>
          <input v-model="retries" class="wb-mono" inputmode="numeric" placeholder="3" />
        </label>
      </div>

      <footer>
        <span class="wb-muted wb-mount-foot-hint">{{ t("syncAdvancedHint") }}</span>
        <span class="wb-mount-foot-actions">
          <button class="wb-dialog-cancel" type="button" @click="emit('close')">{{ t("cancel") }}</button>
          <button class="wb-dialog-primary" type="button" :disabled="!targetPath.trim()" @click="confirm">{{ t("syncConfirm") }}</button>
        </span>
      </footer>
    </div>
  </div>
</template>
