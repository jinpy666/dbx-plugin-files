<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { FolderOpen, Plus, RotateCcw, X } from "@lucide/vue";
import type { OpenAppMapping, OpenAppPrefs } from "../lib/prefs";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  canSaveLocal: boolean;
  saveDir: string;
  defaultSaveDir: string;
  downloadDirError?: string;
  /** 外部打开应用偏好（issue #11）：全局默认 + 按扩展名映射。 */
  openApp: OpenAppPrefs;
  openAppError?: string;
}>();

const emit = defineEmits<{
  (event: "save-dir", dir: string): void;
  (event: "save-open-app", prefs: OpenAppPrefs): void;
}>();

const draft = ref(props.saveDir);
watch(() => props.saveDir, (value) => {
  draft.value = value;
});

// 外部应用偏好：drafts 是编辑期真源。设置页签每次打开都重新挂载，所以只需
// 初始化一次；这里不回写 watch——半成品映射行（只填了扩展名或只填了应用）
// 会被父层过滤掉，若 watch 回写会把用户正在编辑的行清空。
const appDraft = ref(props.openApp.defaultApp);
const mappingDrafts = ref<OpenAppMapping[]>(props.openApp.mappings.map((mapping) => ({ ...mapping })));

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

function emitOpenApp(app: string, mappings: OpenAppMapping[]) {
  emit("save-open-app", { defaultApp: app, mappings });
}

function onAppChange(event: Event) {
  appDraft.value = (event.target as HTMLInputElement).value;
  emitOpenApp(appDraft.value, mappingDrafts.value);
}

function restoreDefaultApp() {
  appDraft.value = "";
  emitOpenApp("", mappingDrafts.value);
}

function onMappingChange(index: number, field: keyof OpenAppMapping, event: Event) {
  const row = mappingDrafts.value[index];
  if (!row) return;
  row[field] = (event.target as HTMLInputElement).value;
  emitOpenApp(appDraft.value, mappingDrafts.value);
}

function addMapping() {
  mappingDrafts.value.push({ ext: "", app: "" });
  emitOpenApp(appDraft.value, mappingDrafts.value);
}

function removeMapping(index: number) {
  mappingDrafts.value.splice(index, 1);
  emitOpenApp(appDraft.value, mappingDrafts.value);
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

    <!-- issue #11：为下载产物指定外部打开应用（默认 + 按扩展名覆盖）。 -->
    <div class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("externalApp") }}</strong>
        <span class="wb-muted">{{ t("settings") }}</span>
      </div>
      <p class="wb-settings-help">{{ t("externalAppHelp") }}</p>

      <template v-if="canSaveLocal">
        <div class="wb-settings-path-row">
          <input
            class="wb-mono"
            :value="appDraft"
            spellcheck="false"
            :placeholder="t('externalAppPlaceholder')"
            :aria-label="t('externalApp')"
            :aria-invalid="Boolean(openAppError)"
            @change="onAppChange"
          />
          <button
            class="wb-icon-button wb-icon-neutral"
            type="button"
            :title="t('useSystemDefaultApp')"
            v-tip="t('useSystemDefaultApp')"
            :disabled="!appDraft"
            @click="restoreDefaultApp"
          ><RotateCcw /></button>
        </div>
        <p v-if="openAppError" class="wb-settings-error" role="alert">{{ openAppError }}</p>

        <p class="wb-settings-help">{{ t("externalAppMappingsHelp") }}</p>
        <div v-for="(mapping, index) in mappingDrafts" :key="index" class="wb-settings-path-row">
          <input
            class="wb-mono"
            :value="mapping.ext"
            spellcheck="false"
            style="flex: 0 0 110px"
            :placeholder="t('extensionPlaceholder')"
            :aria-label="t('extensionColumn')"
            @change="onMappingChange(index, 'ext', $event)"
          />
          <input
            class="wb-mono"
            :value="mapping.app"
            spellcheck="false"
            :placeholder="t('externalAppPlaceholder')"
            :aria-label="t('appColumn')"
            @change="onMappingChange(index, 'app', $event)"
          />
          <button
            class="wb-icon-button wb-icon-neutral"
            type="button"
            :title="t('removeMapping')"
            v-tip="t('removeMapping')"
            @click="removeMapping(index)"
          ><X /></button>
        </div>
        <button class="wb-link-button" type="button" @click="addMapping"><Plus /> {{ t("addMapping") }}</button>
      </template>
      <p v-else class="wb-settings-help">{{ t("downloadDirectoryUnavailable") }}</p>
    </div>
  </section>
</template>
