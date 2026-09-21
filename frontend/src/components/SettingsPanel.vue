<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { FolderOpen, Plus, RotateCcw, X } from "@lucide/vue";
import type { OpenAppMapping, OpenAppPrefs } from "../lib/prefs";

/** 独立设置弹窗按分类只渲染一个 section；不传 section 时两段都渲染（兼容旧用法）。 */
export type SettingsSection = "downloads" | "openWith" | "transfer";

/** 平台预设（files/local/detect-apps 探测到的本机已装应用）。 */
export interface AppPresetOption {
  id: string;
  name: string;
  path: string;
}

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  canSaveLocal: boolean;
  saveDir: string;
  defaultSaveDir: string;
  downloadDirError?: string;
  /** 外部打开应用偏好（issue #11）：全局默认 + 按扩展名映射。 */
  openApp: OpenAppPrefs;
  openAppError?: string;
  section?: SettingsSection;
  /** 本机检测到的应用预设；空数组（旧 sidecar/未探测到）时隐藏预设区。 */
  presets?: AppPresetOption[];
  /** 传输带宽限速（files/bwlimit 持久化值；空 = 不限）。 */
  bwlimit?: string;
  bwlimitError?: string;
}>();

const emit = defineEmits<{
  (event: "save-dir", dir: string): void;
  (event: "save-open-app", prefs: OpenAppPrefs): void;
  (event: "save-bwlimit", rate: string): void;
}>();

// 带宽限速：编辑期真源，显式「保存」触发 sidecar 校验 + 持久化。
const bwlimitDraft = ref(props.bwlimit ?? "");
watch(() => props.bwlimit, (value) => {
  bwlimitDraft.value = value ?? "";
});

// 预设一键填入（保存仍走显式按钮，语义与手输一致）；不限 = 空值。
const BWLIMIT_PRESETS: Array<{ labelKey: string; value: string }> = [
  { labelKey: "bwlimitPlaceholder", value: "" },
  { labelKey: "bwlimitPreset1M", value: "1M" },
  { labelKey: "bwlimitPreset10M", value: "10M" },
  { labelKey: "bwlimitPreset100M", value: "100M" },
];

function applyBwlimitPreset(value: string) {
  bwlimitDraft.value = value;
}

// 输入期软提示：非空且形如限速值之外时立即提示格式，不用等保存被 rclone 拒。
const BWLIMIT_FORMAT = /^\d+(\.\d+)?[kKmMgGtT]?[bB]?(:\d+(\.\d+)?[kKmMgGtT]?[bB]?)?$/;
const bwlimitFormatWarn = computed(() => {
  const value = bwlimitDraft.value.trim();
  return Boolean(value) && value !== "off" && !BWLIMIT_FORMAT.test(value);
});

function saveBwlimit() {
  emit("save-bwlimit", bwlimitDraft.value.trim());
}

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

/** 平台预设一键应用：填入默认应用并走既有校验持久化链路；未装/路径变化由
 * openAppError 行内提示（sidecar 校验是唯一真源）。 */
function applyPreset(preset: AppPresetOption) {
  appDraft.value = preset.path;
  emitOpenApp(preset.path, mappingDrafts.value);
}
</script>

<template>
  <section class="wb-settings-panel" :aria-label="t('settingsPanel')">
    <div v-if="props.section === 'transfer'" class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("bwlimitLabel") }}</strong>
        <span class="wb-muted">{{ t("settings") }}</span>
      </div>
      <p class="wb-settings-help">{{ t("bwlimitHelp") }}</p>
      <div class="wb-settings-path-row">
        <input
          v-model="bwlimitDraft"
          class="wb-mono"
          spellcheck="false"
          :placeholder="t('bwlimitPlaceholder')"
          :aria-label="t('bwlimitLabel')"
          :aria-invalid="Boolean(bwlimitError)"
          @keydown.enter.prevent="saveBwlimit"
        />
        <button class="wb-toolbar-button" type="button" @click="saveBwlimit">{{ t("bwlimitSave") }}</button>
      </div>
      <div class="wb-bwlimit-presets" role="group" :aria-label="t('bwlimitLabel')">
        <button
          v-for="preset in BWLIMIT_PRESETS"
          :key="preset.value || 'off'"
          type="button"
          class="wb-bwlimit-chip"
          :class="{ 'is-active': bwlimitDraft.trim() === preset.value }"
          @click="applyBwlimitPreset(preset.value)"
        >{{ t(preset.labelKey) }}</button>
      </div>
      <p v-if="bwlimitError" class="wb-settings-error" role="alert">{{ bwlimitError }}</p>
      <p v-else-if="bwlimitFormatWarn" class="wb-settings-hint">{{ t("bwlimitFormat") }}</p>
    </div>
    <div v-if="!props.section || props.section === 'downloads'" class="wb-settings-section">
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
    <div v-if="!props.section || props.section === 'openWith'" class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("externalApp") }}</strong>
        <span class="wb-muted">{{ t("settings") }}</span>
      </div>
      <p class="wb-settings-help">{{ t("externalAppHelp") }}</p>

      <template v-if="canSaveLocal">
        <!-- 平台预设（files/local/detect-apps）：本机探测到的 WPS/Excel/... 一键设为默认应用。 -->
        <div v-if="presets?.length" class="wb-presets">
          <span class="wb-muted">{{ t("presetsTitle") }}</span>
          <div class="wb-presets-row">
            <button
              v-for="preset in presets"
              :key="preset.id"
              type="button"
              class="wb-preset-chip"
              :title="preset.path"
              :aria-label="`${t('presetsApply')}: ${preset.name}`"
              @click="applyPreset(preset)"
            >{{ preset.name }}</button>
          </div>
          <p class="wb-settings-help">{{ t("presetsHint") }}</p>
        </div>
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
