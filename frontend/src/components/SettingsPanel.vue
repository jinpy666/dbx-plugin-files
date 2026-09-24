<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { FolderOpen, Plus, RotateCcw, X } from "@lucide/vue";
import type { OpenAppMapping, OpenAppPrefs } from "../lib/prefs";
import { CONFLICT_POLICIES, type ConflictPolicy } from "../lib/conflictPolicy";
import DesktopOnlyCard from "./DesktopOnlyCard.vue";
import DirectoryBrowser from "./DirectoryBrowser.vue";
import { persistDownloadDir } from "../lib/prefs";

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
  /** 传输同名冲突策略（上传预检 + 下载落盘；缺省 ask）。 */
  conflictPolicy?: ConflictPolicy;
}>();

const emit = defineEmits<{
  (event: "save-dir", dir: string): void;
  (event: "save-open-app", prefs: OpenAppPrefs): void;
  (event: "save-bwlimit", rate: string): void;
  (event: "save-conflict-policy", policy: ConflictPolicy): void;
  /** 任一草稿与持久化值不一致时上抛，父层据此启用统一的「保存更改」。 */
  (event: "dirty", dirty: boolean): void;
}>();

// 带宽限速：编辑期真源，显式「保存」触发 sidecar 校验 + 持久化。
const bwlimitDraft = ref(props.bwlimit ?? "");
watch(() => props.bwlimit, (value) => {
  bwlimitDraft.value = value ?? "";
});

// 限速 = 数字 + 单位（KB/s / MB/s / GB/s / TB/s）的组合输入；数字留空 = 不限。
// 保存时合成 rclone 速率串（如 "10M"）。历史值可能是分段限速（"1M:100k"），
// UI 只呈现上半段并在保存时替换为单一限速——明确提示，不做静默丢数据。
const bwlimitNumber = ref<string | number>("");
const bwlimitUnit = ref("M");
const bwlimitSplit = ref(false);

const BWLIMIT_UNITS = ["K", "M", "G", "T"] as const;

function parseBwlimit(raw: string) {
  const value = raw.trim();
  bwlimitSplit.value = value.includes(":");
  const head = (value.split(":")[0] ?? "").trim();
  const match = /^(\d+(?:\.\d+)?)\s*([kKmMgGtT])[bB]?$/.exec(head);
  if (!match) {
    bwlimitNumber.value = "";
    bwlimitUnit.value = "M";
    return;
  }
  bwlimitNumber.value = match[1] ?? "";
  const unit = (match[2] ?? "M").toUpperCase();
  bwlimitUnit.value = (BWLIMIT_UNITS as readonly string[]).includes(unit) ? unit : "M";
}

watch(
  () => props.bwlimit,
  (value) => parseBwlimit(value ?? ""),
  { immediate: true },
);

function saveBwlimit() {
  // type=number 的 v-model 会自动转数值，这里统一回字符串再合成速率串。
  const number = String(bwlimitNumber.value ?? "").trim();
  emit("save-bwlimit", number ? `${number}${bwlimitUnit.value}` : "");
}

// 同名冲突策略：radio 三档（ask/rename/overwrite），统一「保存更改」时持久化。
const CONFLICT_LABEL_KEYS: Record<ConflictPolicy, string> = { ask: "conflictAsk", rename: "conflictRename", overwrite: "conflictOverwrite" };
const conflictDraft = ref<ConflictPolicy>(props.conflictPolicy ?? "ask");
watch(() => props.conflictPolicy, (value) => {
  conflictDraft.value = value ?? "ask";
});

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

function onChange(event: Event) {
  draft.value = (event.target as HTMLInputElement).value;
}

function restoreDefault() {
  draft.value = "";
}

async function pickDirectory() {
  const picker = fileTransfer.value?.pickDirectory;
  if (!picker) return;
  try {
    const result = await picker();
    if (!result) return;
    const path = typeof result === "string" ? result : result.path;
    if (path) draft.value = path;
  } catch {
    // A canceled native picker is intentionally silent.
  }
}

function onAppChange(event: Event) {
  appDraft.value = (event.target as HTMLInputElement).value;
}

function restoreDefaultApp() {
  appDraft.value = "";
}

function onMappingChange(index: number, field: keyof OpenAppMapping, event: Event) {
  const row = mappingDrafts.value[index];
  if (!row) return;
  row[field] = (event.target as HTMLInputElement).value;
}

function addMapping() {
  mappingDrafts.value.push({ ext: "", app: "" });
}

function removeMapping(index: number) {
  mappingDrafts.value.splice(index, 1);
}

/** 平台预设一键应用：填入默认应用草稿；统一「保存更改」时才校验+持久化。 */
function applyPreset(preset: AppPresetOption) {
  appDraft.value = preset.path;
}

// ---- 统一保存（设置弹窗底部「保存更改」）--------------------------------
// 草稿与持久化值是否一致；openWith 与 App 层共用同一清洗规则（trim/去点/小写/
// 过滤半成品行），保证保存后 dirty 精确归零。
function normalizeOpenApp() {
  return {
    defaultApp: appDraft.value.trim(),
    mappings: mappingDrafts.value
      .map((mapping) => ({
        ext: mapping.ext.trim().replace(/^\.+/, "").toLowerCase(),
        app: mapping.app.trim(),
      }))
      .filter((mapping) => mapping.ext && mapping.app),
  };
}

function sameOpenApp(a: OpenAppPrefs, b: OpenAppPrefs): boolean {
  return a.defaultApp === b.defaultApp && JSON.stringify(a.mappings) === JSON.stringify(b.mappings);
}

const canSaveLocal = computed(() => props.canSaveLocal);

const composedBwlimit = computed(() => {
  const number = String(bwlimitNumber.value ?? "").trim();
  return number ? `${number}${bwlimitUnit.value}` : "";
});

const dirty = computed(() => {
  if (props.section === "transfer") {
    return composedBwlimit.value !== (props.bwlimit ?? "") || conflictDraft.value !== (props.conflictPolicy ?? "ask");
  }
  if (props.section === "downloads") return draft.value !== (props.saveDir ?? "");
  if (props.section === "openWith") return !sameOpenApp(normalizeOpenApp(), props.openApp);
  return false;
});

watch(dirty, (value) => emit("dirty", value), { immediate: true });

/** 统一保存入口（设置弹窗底部按钮）：把当前区块的草稿交给父层的既有校验/
 * 持久化链路；行内错误仍由各区块就地展示。 */
async function save() {
  if (props.section === "transfer") {
    emit("save-bwlimit", composedBwlimit.value);
    emit("save-conflict-policy", conflictDraft.value);
    return;
  }
  if (props.section === "downloads") {
    if (canSaveLocal.value) emit("save-dir", draft.value);
    return;
  }
  if (props.section === "openWith") {
    if (canSaveLocal.value) emit("save-open-app", normalizeOpenApp());
    return;
  }
}

defineExpose({ save });
</script>

<template>
  <section class="wb-settings-panel" :aria-label="t('settingsPanel')">
    <div v-if="props.section === 'transfer'" class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("bwlimitLabel") }}</strong>
      </div>
      <p class="wb-settings-help">{{ t("bwlimitHelp") }}</p>
      <div class="wb-bwlimit-combo">
        <input
          v-model="bwlimitNumber"
          class="wb-mono"
          type="number"
          min="0"
          step="any"
          inputmode="decimal"
          :placeholder="t('bwlimitPlaceholder')"
          :aria-label="t('bwlimitLabel')"
          :aria-invalid="Boolean(bwlimitError)"
        />
        <select v-model="bwlimitUnit" class="wb-bwlimit-unit" :aria-label="t('bwlimitUnitLabel')">
          <option value="K">{{ t("bwlimitUnitK") }}</option>
          <option value="M">{{ t("bwlimitUnitM") }}</option>
          <option value="G">{{ t("bwlimitUnitG") }}</option>
          <option value="T">{{ t("bwlimitUnitT") }}</option>
        </select>
      </div>
      <p v-if="bwlimitError" class="wb-settings-error" role="alert">{{ bwlimitError }}</p>
      <p v-else-if="bwlimitSplit" class="wb-settings-hint">{{ t("bwlimitSplitHint") }}</p>

      <!-- 传输同名冲突策略（对标 ssh 插件 downloadConflictPolicy）：上传预检与
           下载落盘共用同一档位。 -->
      <div class="wb-settings-heading">
        <strong>{{ t("conflictPolicyTitle") }}</strong>
      </div>
      <div class="wb-conflict-options" role="radiogroup" :aria-label="t('conflictPolicyTitle')">
        <label v-for="policy in CONFLICT_POLICIES" :key="policy" class="wb-conflict-option">
          <input v-model="conflictDraft" type="radio" name="files-conflict-policy" :value="policy" />
          <span>{{ t(CONFLICT_LABEL_KEYS[policy]) }}</span>
        </label>
      </div>
      <p class="wb-settings-hint">{{ t("conflictPolicyHint") }}</p>
    </div>
    <div v-if="!props.section || props.section === 'downloads'" class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("downloadDirectory") }}</strong>
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
          @input="onChange"
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
      <!-- 内嵌本机目录浏览器（与挂载对话框共用）：点选目录即回填，统一保存时持久化。 -->
      <DirectoryBrowser v-if="canSaveLocal" :t="t" @navigate="draft = $event" />
      <DesktopOnlyCard v-else :t="t" />
    </div>

    <!-- issue #11：为下载产物指定外部打开应用（默认 + 按扩展名覆盖）。 -->
    <div v-if="!props.section || props.section === 'openWith'" class="wb-settings-section">
      <div class="wb-settings-heading">
        <strong>{{ t("externalApp") }}</strong>
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
      <DesktopOnlyCard v-else :t="t" />
    </div>
  </section>
</template>
