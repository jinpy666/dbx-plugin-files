<script setup lang="ts">
// audit.jsonl 只读视图：审计条目由 sidecar store（F-A）经 files/audit/list 透出；
// 方法未就绪时展示不可用态而非报错。
import { onMounted, ref } from "vue";

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
  const at = typeof entry.at === "string" ? entry.at : "";
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
    <template v-if="available === false">
      <div class="wb-file-empty">{{ t("auditUnavailable") }}</div>
      <div class="wb-muted" style="padding: 0 8px; font-size: 11px">{{ t("auditHint") }}</div>
    </template>
    <div v-else-if="!entries.length" class="wb-file-empty">{{ loading ? "…" : t("auditEmpty") }}</div>
    <div v-for="(entry, index) in entries" v-else :key="index" class="wb-audit-item">
      <div class="wb-audit-head"><span>{{ head(entry) }}</span></div>
      <div class="wb-audit-path wb-mono">{{ detail(entry) }}</div>
    </div>
  </div>
</template>
