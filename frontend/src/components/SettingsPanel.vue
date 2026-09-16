<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { FolderOpen, RotateCcw } from "@lucide/vue";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  canSaveLocal: boolean;
  saveDir: string;
  defaultSaveDir: string;
  downloadDirError?: string;
}>();

const emit = defineEmits<{
  (event: "save-dir", dir: string): void;
}>();

const draft = ref(props.saveDir);
watch(() => props.saveDir, (value) => {
  draft.value = value;
});

const fileTransfer = computed(() => window.dbxPlugin?.fileTransfer);
const canPickDirectory = computed(() => typeof fileTransfer.value?.pickDirectory === "function");

function save(value: string) {
  draft.value = value;
  emit("save-dir", value);
}

function onChange(event: Event) {
  save((event.target as HTMLInputElement).value);
}

function restoreDefault() {
  save("");
}

async function pickDirectory() {
  const picker = fileTransfer.value?.pickDirectory;
  if (!picker) return;
  try {
    const result = await picker();
    if (!result) return;
    const path = typeof result === "string" ? result : result.path;
    if (path) save(path);
  } catch {
    // A canceled native picker is intentionally silent.
  }
}
</script>

<template>
  <section class="wb-settings-panel" :aria-label="t('settingsPanel')">
    <div class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("downloadDirectory") }}</strong>
        <span class="wb-muted">{{ t("settings") }}</span>
      </div>
      <p class="wb-settings-help">{{ t("downloadDirectoryHelp") }}</p>

      <div v-if="canSaveLocal" class="wb-settings-path-row">
        <input
          class="wb-mono"
          :value="draft"
          spellcheck="false"
          :placeholder="defaultSaveDir || t('saveToDefault')"
          :aria-label="t('downloadDirectory')"
          :aria-invalid="Boolean(downloadDirError)"
          @change="onChange"
        />
        <button
          v-if="canPickDirectory"
          class="wb-icon-button wb-icon-neutral"
          type="button"
          :title="t('chooseDirectory')"
          v-tip="t('chooseDirectory')"
          @click="pickDirectory"
        ><FolderOpen /></button>
        <button
          class="wb-icon-button wb-icon-neutral"
          type="button"
          :title="t('restoreDefaultDirectory')"
          v-tip="t('restoreDefaultDirectory')"
          :disabled="!draft"
          @click="restoreDefault"
        ><RotateCcw /></button>
      </div>
      <p v-if="downloadDirError" class="wb-settings-error" role="alert">{{ downloadDirError }}</p>
      <p v-if="canSaveLocal" class="wb-settings-default wb-mono">
        {{ draft ? draft : `${t("usingDefaultDirectory")}: ${defaultSaveDir || t("saveToDefault")}` }}
      </p>
      <p v-else class="wb-settings-help">{{ t("downloadDirectoryUnavailable") }}</p>
    </div>
  </section>
</template>
