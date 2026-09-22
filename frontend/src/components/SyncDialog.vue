<script setup lang="ts">
// 目录同步/复制选项对话框（独立顶层弹窗，同 MountDialog 形态）：源/目标路径
// （均可编辑，带共享目录选择器）+ dry-run 预览 + include/exclude 过滤 +
// backup-dir/suffix 备份 + 高级并发参数。确认时把非空字段打包成
// SyncDialogOptions 交给父层发起 files/syncDir|copyDir|bisync；数值字段在
// 组件内先夹紧范围（rc 对非法值静默忽略，夹紧是唯一防线）。路径提交
// （change）时上抛 pair-change，父层据此刷新 bisync 状态查询。
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { FolderOpen, X } from "@lucide/vue";
import { parentPath } from "../lib/api";
import DirectoryBrowser from "./DirectoryBrowser.vue";

/** 确认载荷：空串/空数组/null = 不传该字段（保持 rclone 默认）。 */
export interface SyncDialogOptions {
  sourcePath: string;
  targetPath: string;
  dryRun: boolean;
  include: string[];
  exclude: string[];
  backupDir: string;
  suffix: string;
  /** --metadata：保留对象元数据；false = 不传。 */
  metadata: boolean;
  /** --update：跳过目标上同尺寸同修改时间的文件（只追加/更新较新者）；false = 不传。 */
  update: boolean;
  /** --existing：只传输目标端已存在的文件（不新增）；false = 不传。 */
  existing: boolean;
  /** --immutable：目标已存在文件视为不可变，跳过且不校验；false = 不传。 */
  immutable: boolean;
  /** --min-size/--max-size（如 "100k"/"1M"）；空串 = 不传。 */
  minSize: string;
  maxSize: string;
  /** --min-age/--max-age（如 "1d"、"2024-01-01"）；空串 = 不传。 */
  minAge: string;
  maxAge: string;
  transfers: number | null;
  checkers: number | null;
  retries: number | null;
  /** 双向同步：本次是否以 resync 模式初始化/修复（破坏性）。 */
  bisyncResync: boolean;
}

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  kind: "syncDir" | "copyDir" | "bisync";
  sourcePath: string;
  defaultTarget: string;
  /** 双向同步状态：null = 查询中；"new" = 首次（必须 resync）；"synced" = 增量。 */
  bisyncState?: "synced" | "new" | null;
  /** 路径选择器浏览的连接（源/目标同连接，由父层固化）。 */
  connectionId: string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  (event: "confirm", options: SyncDialogOptions): void;
  /** 路径对提交（change）：父层对 bisync 重新查询 pair 状态。 */
  (event: "pair-change", pair: { sourcePath: string; targetPath: string }): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

// Esc 关闭：与 ConfirmDialog/MountDialog 的键盘语义对齐。
function onDialogKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") emit("close");
}
onMounted(() => window.addEventListener("keydown", onDialogKeydown));
onBeforeUnmount(() => window.removeEventListener("keydown", onDialogKeydown));

// 源路径默认取右键目录，可编辑重指；目标默认同连接根下同名目录。
const sourcePath = ref(props.sourcePath);
const targetPath = ref(props.defaultTarget);
const dryRun = ref(false);
const bisyncResync = ref(false);
// 首次运行（无状态）：resync 是唯一入口，直接勾上且不可取消。状态是弹窗
// 打开后异步取回的（初始 null），prop 晚到必须用 watch 兜住。
if (props.kind === "bisync" && props.bisyncState === "new") bisyncResync.value = true;
watch(
  () => props.bisyncState,
  (state) => {
    if (props.kind === "bisync" && state === "new") bisyncResync.value = true;
  },
);
const include = ref("");
const exclude = ref("");
const backupDir = ref("");
const suffix = ref("");
const advancedOpen = ref(false);
const metadata = ref(false);
const update = ref(false);
const existing = ref(false);
const immutable = ref(false);
const minSize = ref("");
const maxSize = ref("");
const minAge = ref("");
const maxAge = ref("");
const transfers = ref("");
const checkers = ref("");
const retries = ref("");

// ---- 共享目录选择器：两个路径字段共用一个内嵌浏览器，browseField 指明
// 当前在为哪个字段挑选；导航（进入子目录）即回填该字段。 ------------------
type PathField = "source" | "target";
const browseField = ref<PathField | null>(null);
const browserStart = ref("");

function toggleBrowse(field: PathField) {
  if (browseField.value === field) {
    browseField.value = null;
    return;
  }
  browseField.value = field;
  // 从当前值父目录起步（根/空值落回根）；父目录不存在时浏览器给出中性提示。
  const current = (field === "source" ? sourcePath : targetPath).value.trim();
  browserStart.value = current ? parentPath(current) : "/";
}

function onBrowseNavigate(path: string) {
  if (browseField.value === "source") sourcePath.value = path;
  else if (browseField.value === "target") targetPath.value = path;
}

/** 路径提交（change 事件，Enter/失焦触发）：通知父层路径对变化。 */
function onPathCommit() {
  emit("pair-change", { sourcePath: sourcePath.value.trim(), targetPath: targetPath.value.trim() });
}

/** 源 = 目标（trim 后相等）：同步/复制无意义，bisync 会自配对——前置拦截
 * 而不是等 rc 报 "Destination must differ from the source"。 */
const sameTarget = computed(() => {
  const source = sourcePath.value.trim();
  return Boolean(source) && source === targetPath.value.trim();
});

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
    sourcePath: sourcePath.value.trim(),
    targetPath: targetPath.value.trim(),
    dryRun: dryRun.value,
    include: parsePatterns(include.value),
    exclude: parsePatterns(exclude.value),
    backupDir: backupDir.value.trim(),
    suffix: suffix.value.trim(),
    metadata: metadata.value,
    update: update.value,
    existing: existing.value,
    immutable: immutable.value,
    minSize: minSize.value.trim(),
    maxSize: maxSize.value.trim(),
    minAge: minAge.value.trim(),
    maxAge: maxAge.value.trim(),
    transfers: parseCount(transfers.value, 1, 32),
    checkers: parseCount(checkers.value, 1, 64),
    retries: parseCount(retries.value, 1, 10),
    bisyncResync: props.kind === "bisync" ? bisyncResync.value : false,
  });
}
</script>

<template>
  <div class="wb-mount-backdrop" role="dialog" aria-modal="true" :aria-label="t('syncOptionsTitle')" @click.self="emit('close')">
    <div class="wb-mount-dialog wb-sync-dialog">
      <header>
        <strong>{{ kind === "bisync" ? t("transferKind.bisync") : kind === "syncDir" ? t("transferKind.syncDir") : t("transferKind.copyDir") }}</strong>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="emit('close')"><X /></button>
      </header>
      <p v-if="kind === 'bisync'" class="wb-mount-hint">{{ t("bisyncBetaWarn") }}</p>
      <p v-else class="wb-mount-hint">{{ kind === "syncDir" ? t("syncDirBody") : t("copyDirBody") }}</p>

      <template v-if="kind === 'bisync'">
        <div class="wb-sync-note">
          <label class="wb-sync-check">
            <input v-model="bisyncResync" type="checkbox" :disabled="bisyncState === 'new'" />
            <span>{{ t("bisyncResyncLabel") }}</span>
          </label>
          <p v-if="bisyncState === 'new'" class="wb-sync-note-text">{{ t("bisyncFirstRun") }}</p>
          <p v-if="bisyncResync" class="wb-sync-note-text">{{ t("bisyncResyncWarn") }}</p>
        </div>
      </template>

      <div class="wb-mount-field">
        <span>{{ t("syncSourceLabel") }}</span>
        <div class="wb-sync-path-row">
          <input
            v-model="sourcePath"
            class="wb-mono"
            spellcheck="false"
            :aria-label="t('syncSourceLabel')"
            @change="onPathCommit"
            @keydown.enter.prevent="onPathCommit"
          />
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
        <span>{{ t("syncTargetLabel") }}</span>
        <div class="wb-sync-path-row">
          <input
            v-model="targetPath"
            class="wb-mono"
            spellcheck="false"
            :placeholder="defaultTarget"
            :aria-label="t('syncTargetLabel')"
            @change="onPathCommit"
            @keydown.enter.prevent="onPathCommit"
          />
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
        :key="`${browseField}:${browserStart}`"
        :t="t"
        :connection-id="connectionId"
        :initial-path="browserStart"
        missing-hint-key="syncBrowseMissing"
        @navigate="onBrowseNavigate"
      />

      <p v-if="sameTarget" class="wb-mount-hint wb-sync-warning" role="alert">{{ t("destMustDiffer") }}</p>

      <label class="wb-sync-check">
        <input v-model="dryRun" type="checkbox" />
        <span>{{ t("syncDryRunLabel") }}</span>
      </label>
      <p v-if="dryRun" class="wb-mount-hint">{{ t("syncDryRunHint") }}</p>

      <div v-if="kind !== 'bisync'" class="wb-sync-grid">
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
        <p class="wb-mount-hint wb-sync-grid-hint">{{ t("syncBackupDirHint") }}</p>
      </div>

      <button v-if="kind !== 'bisync'" type="button" class="wb-sync-advanced-toggle" @click="advancedOpen = !advancedOpen">
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
        <label class="wb-mount-field">
          <span>{{ t("syncMinSizeLabel") }}</span>
          <input v-model="minSize" class="wb-mono" spellcheck="false" placeholder="100k" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncMaxSizeLabel") }}</span>
          <input v-model="maxSize" class="wb-mono" spellcheck="false" placeholder="1M" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncMinAgeLabel") }}</span>
          <input v-model="minAge" class="wb-mono" spellcheck="false" placeholder="1d" />
        </label>
        <label class="wb-mount-field">
          <span>{{ t("syncMaxAgeLabel") }}</span>
          <input v-model="maxAge" class="wb-mono" spellcheck="false" placeholder="1d" />
        </label>
        <label class="wb-sync-check">
          <input v-model="metadata" type="checkbox" />
          <span>{{ t("syncMetadataLabel") }}</span>
        </label>
        <!-- rclone 策略旗标（--update/--existing/--immutable）：仅 true 时
             下发，缺省保持 rclone 默认比对/新增/覆盖语义。 -->
        <label class="wb-sync-check">
          <input v-model="update" type="checkbox" />
          <span>{{ t("syncUpdateLabel") }}</span>
        </label>
        <label class="wb-sync-check">
          <input v-model="existing" type="checkbox" />
          <span>{{ t("syncExistingLabel") }}</span>
        </label>
        <label class="wb-sync-check">
          <input v-model="immutable" type="checkbox" />
          <span>{{ t("syncImmutableLabel") }}</span>
        </label>
      </div>

      <footer>
        <span v-if="kind !== 'bisync'" class="wb-muted wb-mount-foot-hint">{{ t("syncAdvancedHint") }}</span>
        <span class="wb-mount-foot-actions">
          <button class="wb-dialog-cancel" type="button" @click="emit('close')">{{ t("cancel") }}</button>
          <button class="wb-dialog-primary" type="button" :disabled="!targetPath.trim() || sameTarget" @click="confirm">{{ t("syncConfirm") }}</button>
        </span>
      </footer>
    </div>
  </div>
</template>
