<script setup lang="ts">
// 批量重命名抽屉（parity-tools，对标 rclone-ui）：查找/替换（正则/忽略大小写）、
// 前缀/后缀、序号；计划表实时预览旧名 → 新名并逐行标错（空名/重复/目录已存在）。
// 应用时按行串行 files/rename（与单文件 rename 同一调用链路），逐行回报成败。
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { call, errorMessage, joinPath, parentPath, type FileEntry } from "../lib/api";
import { buildRenamePlan, type RenamePlanOptions } from "../lib/batchRename";
import { trapTabKey } from "../lib/a11y";

const props = defineProps<{
  /** 参与重命名的条目（多选集）。 */
  entries: FileEntry[];
  /** 所在目录的全体条目名（目录内重名预检用）。 */
  siblingNames: string[];
  /** 发起栏的连接快照（打开时固化）。 */
  connectionId: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  /** 应用结束（无论成败）：App 侧负责汇总通知与目录刷新。 */
  (event: "applied", result: { ok: number; total: number }): void;
}>();

const find = ref("");
const replace = ref("");
const regex = ref(false);
const ignoreCase = ref(false);
const prefix = ref("");
const suffix = ref("");
const numbering = ref(false);
const numberingStart = ref(1);
const applying = ref(false);
/** 应用结果：path → "ok" | 错误消息（仅已尝试行）。 */
const statuses = ref<Record<string, string>>({});

const drawerEl = ref<HTMLElement>();

const options = computed<RenamePlanOptions>(() => ({
  find: find.value,
  replace: replace.value,
  regex: regex.value,
  ignoreCase: ignoreCase.value,
  prefix: prefix.value,
  suffix: suffix.value,
  numbering: numbering.value,
  numberingStart: numberingStart.value,
}));

const plan = computed(() => buildRenamePlan(props.entries, options.value, props.siblingNames));

const renameCount = computed(() => plan.value.applicable);

function statusOf(path: string): string {
  return statuses.value[path] ?? "";
}

/** 应用：串行 files/rename（沿用单文件 rename 的调用链与错误透传）。 */
async function apply() {
  if (applying.value || !renameCount.value) return;
  applying.value = true;
  statuses.value = {};
  const targets = plan.value.rows.filter((row) => row.changed && !row.error);
  let ok = 0;
  for (const row of targets) {
    try {
      const newPath = joinPath(parentPath(row.entry.path), row.newName);
      // 目录 rename 可能降级 job（transport=job）：任务继续在传输面板跟踪，行内按成功计。
      await call<{ transport?: string; jobId?: string | null }>("files/rename", {
        connectionId: props.connectionId,
        path: row.entry.path,
        newPath,
      });
      statuses.value = { ...statuses.value, [row.entry.path]: "ok" };
      ok += 1;
    } catch (cause) {
      statuses.value = { ...statuses.value, [row.entry.path]: errorMessage(cause) };
    }
  }
  applying.value = false;
  emit("applied", { ok, total: targets.length });
}

/** 焦点陷阱：Tab 在抽屉内循环（同 ConfirmDialog）。 */
function onTabKeydown(event: KeyboardEvent) {
  if (event.key !== "Tab") return;
  trapTabKey(event, drawerEl.value);
}

watch(
  () => props.entries,
  () => {
    statuses.value = {};
  },
);

onMounted(async () => {
  await nextTick();
  drawerEl.value?.querySelector<HTMLInputElement>("input")?.focus();
});
</script>

<template>
  <div class="wb-dialog-backdrop" @click.self="emit('close')">
    <div ref="drawerEl" class="wb-dialog wb-batch-drawer" role="dialog" aria-modal="true" :aria-label="t('batchRenameTitle')" @keydown="onTabKeydown">
      <header>
        <strong>{{ t("batchRenameTitle") }}</strong>
      </header>
      <div class="wb-batch-form">
        <label>
          <span>{{ t("batchRenameFind") }}</span>
          <input v-model="find" type="text" spellcheck="false" data-test="find" />
        </label>
        <label>
          <span>{{ t("batchRenameReplace") }}</span>
          <input v-model="replace" type="text" spellcheck="false" data-test="replace" />
        </label>
        <label class="wb-batch-check">
          <input v-model="regex" type="checkbox" data-test="regex" />
          <span>{{ t("batchRenameRegex") }}</span>
        </label>
        <label class="wb-batch-check">
          <input v-model="ignoreCase" type="checkbox" data-test="ignore-case" />
          <span>{{ t("batchRenameIgnoreCase") }}</span>
        </label>
        <label>
          <span>{{ t("batchRenamePrefix") }}</span>
          <input v-model="prefix" type="text" spellcheck="false" data-test="prefix" />
        </label>
        <label>
          <span>{{ t("batchRenameSuffix") }}</span>
          <input v-model="suffix" type="text" spellcheck="false" data-test="suffix" />
        </label>
        <label class="wb-batch-check">
          <input v-model="numbering" type="checkbox" data-test="numbering" />
          <span>{{ t("batchRenameNumbering") }}</span>
        </label>
        <label v-if="numbering">
          <span>{{ t("batchRenameNumberStart") }}</span>
          <input v-model.number="numberingStart" type="number" min="0" data-test="numbering-start" />
        </label>
      </div>
      <p v-if="plan.invalidRegex" class="wb-dialog-warning" role="alert" style="padding: 0 14px">{{ t("batchRenameErrorRegex") }}</p>
      <div class="wb-batch-plan">
        <table>
          <thead>
            <tr>
              <th>{{ t("batchRenameColumnOld") }}</th>
              <th>{{ t("batchRenameColumnNew") }}</th>
              <th style="width: 34%"></th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="row in plan.rows"
              :key="row.entry.path"
              :class="{ 'is-error': row.error, 'is-ok': statusOf(row.entry.path) === 'ok' }"
              :data-test="`plan-row`"
            >
              <td>{{ row.oldName }}</td>
              <td>
                <span class="wb-batch-arrow" aria-hidden="true">{{ row.changed ? "→" : "=" }}</span>
                {{ row.newName }}
              </td>
              <td>
                <template v-if="statusOf(row.entry.path) === 'ok'">{{ t("batchRenameOk") }}</template>
                <template v-else-if="statusOf(row.entry.path)">{{ statusOf(row.entry.path) }}</template>
                <template v-else-if="row.error">{{ t(row.error) }}</template>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <footer>
        <span class="wb-muted">{{ renameCount ? "" : t("batchRenameNoChanges") }}</span>
        <button type="button" class="wb-dialog-cancel" :disabled="applying" @click="emit('close')">{{ t("close") }}</button>
        <button type="button" class="wb-dialog-primary" :disabled="!renameCount || applying" data-test="apply" @click="apply">
          {{ applying ? t("loading") : t("batchRenameApply", { count: renameCount }) }}
        </button>
      </footer>
    </div>
  </div>
</template>
