<script setup lang="ts">
// audit.jsonl 只读视图：审计条目由 sidecar store（F-A）经 files/audit/list 透出；
// 方法未就绪时展示不可用态而非报错。
import { computed, onMounted, ref, watch } from "vue";
import { RefreshCw } from "@lucide/vue";
import { formatTime } from "../lib/api";
import { loadUiPrefs, saveUiPrefs } from "../lib/prefs";

export interface AuditEntry {
  at: string;
  method?: string;
  action?: string;
  path?: string;
  connectionId?: string;
  result?: string;
  [key: string]: unknown;
}

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const entries = ref<AuditEntry[]>([]);
/** 操作类型筛选：候选来自当前记录里出现过的 action（全部 = 空串）。 */
const actionFilter = ref(loadUiPrefs().auditActionFilter ?? "");
watch(actionFilter, (value) => {
  saveUiPrefs({ ...loadUiPrefs(), auditActionFilter: value });
});

const actionOptions = computed(() => {
  const seen = new Set<string>();
  for (const entry of entries.value) {
    const action = entry.action ?? entry.method ?? "";
    if (action) seen.add(action);
  }
  return [...seen].sort((a, b) => a.localeCompare(b));
});

const filteredEntries = computed(() =>
  actionFilter.value
    ? entries.value.filter((entry) => (entry.action ?? entry.method ?? "") === actionFilter.value)
    : entries.value,
);
const available = ref<boolean | undefined>(undefined);
const loading = ref(false);

async function refresh() {
  loading.value = true;
  try {
    const result = await window.dbxPlugin.invoke<{ entries?: AuditEntry[] }>("files/audit/list", { limit: 100 });
    entries.value = result.entries ?? [];
    available.value = true;
  } catch (cause) {
    if (/method not found/i.test(String(cause))) available.value = false;
    else available.value = false;
  } finally {
    loading.value = false;
  }
}

function head(entry: AuditEntry): string {
  // P2-5：ISO 原文 → 本地化 "YYYY-MM-DD HH:mm"；非时间串原样展示。
  const at = formatTime(typeof entry.at === "string" ? entry.at : undefined) || (typeof entry.at === "string" ? entry.at : "");
  const action = entry.action ?? entry.method ?? "";
  return `${at} ${action}`.trim();
}

function detail(entry: AuditEntry): string {
  return [entry.result, entry.connectionId, entry.path].filter((value): value is string => typeof value === "string" && !!value).join(" · ");
}

onMounted(refresh);
defineExpose({ refresh });
props;
</script>

<template>
  <div class="wb-audit-list">
    <!-- P2-5：手动刷新钮——面板打开期间错过的写入不再要求切 tab 触发 watch -->
    <div class="wb-transfer-history-head" style="margin: 2px 0 6px">
      <span class="wb-muted">{{ t("auditPanel") }}</span>
      <button class="wb-icon-button" v-tip="t('refresh')" :disabled="loading" @click="refresh">
        <RefreshCw :class="{ 'wb-spin': loading }" />
      </button>
    </div>
    <template v-if="available === false">
      <div class="wb-file-empty">{{ t("auditUnavailable") }}</div>
      <div class="wb-muted" style="padding: 0 8px; font-size: 11px">{{ t("auditHint") }}</div>
    </template>
    <template v-else>
      <label class="wb-audit-filter">
        <span class="wb-muted">{{ t("auditFilterLabel") }}</span>
        <select v-model="actionFilter" :aria-label="t('auditFilterLabel')">
          <option value="">{{ t("auditFilterAll") }}</option>
          <option v-for="action in actionOptions" :key="action" :value="action">{{ action }}</option>
        </select>
      </label>
      <div v-if="!entries.length" class="wb-file-empty">{{ loading ? "…" : t("auditEmpty") }}</div>
      <p v-else-if="!filteredEntries.length" class="wb-file-empty">{{ t("auditFilterNoMatch") }}</p>
      <div v-for="(entry, index) in filteredEntries" v-else :key="index" class="wb-audit-item">
        <div class="wb-audit-head"><span>{{ head(entry) }}</span></div>
        <div class="wb-audit-path wb-mono">{{ detail(entry) }}</div>
      </div>
    </template>
  </div>
</template>
