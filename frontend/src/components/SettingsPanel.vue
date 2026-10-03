<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { FolderOpen, Plus, RotateCcw, X } from "@lucide/vue";
import type { OpenAppMapping, OpenAppPrefs } from "../lib/prefs";
import { CONFLICT_POLICIES, type ConflictPolicy } from "../lib/conflictPolicy";
import {
  sanitizeEditorConfig,
  type EditorAssociation,
  type EditorConfig,
  type KnownEditor,
} from "../lib/editorRules";
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
  /** 外部编辑器关联配置（1:1 复刻 ssh sftpEdit 设置区）。 */
  editorConfig: EditorConfig;
  /** sidecar 编辑器目录（files/local/editors/list；旧 sidecar 为空数组）。 */
  editors: KnownEditor[];
}>();

const emit = defineEmits<{
  (event: "save-dir", dir: string): void;
  (event: "save-open-app", prefs: OpenAppPrefs): void;
  (event: "save-bwlimit", rate: string): void;
  (event: "save-conflict-policy", policy: ConflictPolicy): void;
  (event: "save-editor-config", config: EditorConfig): void;
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

// ---- 外部编辑器关联草稿（1:1 复刻 ssh sftpEdit 设置区）--------------------
// 配置整体草稿化、随主「保存更改」落库；sanitize 产出全新对象，不与父层
// 共享引用。默认编辑器用 "system"/"known:<id>"/"custom:<id>" 哨兵编码
// （与 ssh SettingsDialog 同形），保存时拆回 EditorConfig 字段。
const editorConfigDraft = ref<EditorConfig>(sanitizeEditorConfig(props.editorConfig));
const editorDefaultDraft = ref(editorDefaultValue(sanitizeEditorConfig(props.editorConfig)));
const editorAssociationPatternDraft = ref("");
const editorAssociationTargetDraft = ref("");

type EditorTargetValue = "system" | `known:${string}` | `custom:${string}`;

function editorDefaultValue(config: EditorConfig): EditorTargetValue {
  return config.defaultCustomId
    ? `custom:${config.defaultCustomId}`
    : config.defaultEditorId
      ? `known:${config.defaultEditorId}`
      : /* 原生 select 不接受空串 value，用 "system" 哨兵表示系统默认 */ "system";
}

/** 可选目标：目录可用项（known）+ 自定义编辑器（custom）；不可用条目不进
 * 新建关联的目标表，但既有关联行仍按名字展示（装回后可重新解析）。 */
const editorTargetOptions = computed(() => [
  ...props.editors
    .filter((editor) => editor.available)
    .map((editor) => ({ value: `known:${editor.id}` as const, label: editor.name })),
  ...editorConfigDraft.value.customEditors.map((editor) => ({ value: `custom:${editor.id}` as const, label: editor.name })),
]);

function editorTargetName(target: string): string {
  if (target.startsWith("custom:")) {
    const id = target.slice(7);
    return editorConfigDraft.value.customEditors.find((editor) => editor.id === id)?.name || id;
  }
  if (target.startsWith("known:")) {
    const id = target.slice(6);
    return props.editors.find((editor) => editor.id === id)?.name || id;
  }
  return target;
}

function associationTargetValue(assoc: EditorAssociation): string {
  return assoc.customId ? `custom:${assoc.customId}` : assoc.editorId ? `known:${assoc.editorId}` : "";
}

function onAssociationPatternChange(event: Event) {
  editorAssociationPatternDraft.value = (event.target as HTMLInputElement).value;
}

function onAssociationTargetChange(event: Event) {
  editorAssociationTargetDraft.value = (event.target as HTMLSelectElement).value;
}

function addEditorAssociation() {
  const pattern = editorAssociationPatternDraft.value.trim();
  const target = editorAssociationTargetDraft.value;
  if (!pattern || !target) return;
  const assoc: EditorAssociation = target.startsWith("custom:")
    ? { pattern, customId: target.slice("custom:".length) }
    : { pattern, editorId: target.slice("known:".length) };
  editorConfigDraft.value = { ...editorConfigDraft.value, associations: [...editorConfigDraft.value.associations, assoc] };
  editorAssociationPatternDraft.value = "";
  editorAssociationTargetDraft.value = "";
}

function removeEditorAssociation(index: number) {
  editorConfigDraft.value = {
    ...editorConfigDraft.value,
    associations: editorConfigDraft.value.associations.filter((_, i) => i !== index),
  };
}

/** 删除自定义编辑器：指向它的关联一并清理（ssh 同形），默认编辑器指向它时
 * 回落系统默认。 */
function removeCustomEditor(id: string) {
  editorConfigDraft.value = {
    ...editorConfigDraft.value,
    customEditors: editorConfigDraft.value.customEditors.filter((editor) => editor.id !== id),
    associations: editorConfigDraft.value.associations.filter((assoc) => assoc.customId !== id),
  };
  if (editorDefaultDraft.value === `custom:${id}`) editorDefaultDraft.value = "system";
}

/** 草稿 → 完整配置：默认编辑器哨兵拆回字段，净化后交给父层持久化。 */
function composedEditorConfig(): EditorConfig {
  const next = { ...editorConfigDraft.value };
  if (editorDefaultDraft.value === "system") {
    next.defaultEditorId = undefined;
    next.defaultCustomId = undefined;
  } else if (editorDefaultDraft.value.startsWith("custom:")) {
    next.defaultCustomId = editorDefaultDraft.value.slice("custom:".length);
    next.defaultEditorId = undefined;
  } else if (editorDefaultDraft.value.startsWith("known:")) {
    next.defaultEditorId = editorDefaultDraft.value.slice("known:".length);
    next.defaultCustomId = undefined;
  }
  return sanitizeEditorConfig(next);
}

function sameEditorConfig(a: EditorConfig, b: EditorConfig): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

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
  if (props.section === "openWith") {
    return (
      !sameOpenApp(normalizeOpenApp(), props.openApp) ||
      !sameEditorConfig(composedEditorConfig(), sanitizeEditorConfig(props.editorConfig))
    );
  }
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
    if (canSaveLocal.value) {
      emit("save-open-app", normalizeOpenApp());
      emit("save-editor-config", composedEditorConfig());
    }
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

        <!-- 外部编辑器关联（1:1 复刻 ssh sftpEdit 设置区）：默认编辑器、保存
             回传策略、关联表与自定义编辑器管理；自定义编辑器的添加入口在
             右键「打开方式 → 自定义命令…」（与 ssh 同形），此处只做管理。 -->
        <div class="wb-settings-heading">
          <strong>{{ t("sftpEdit.settingsSection") }}</strong>
        </div>
        <label class="wb-editor-field">
          <span class="wb-muted">{{ t("sftpEdit.defaultEditorTitle") }}</span>
          <select v-model="editorDefaultDraft" :aria-label="t('sftpEdit.defaultEditorTitle')">
            <option value="system">{{ t("sftpEdit.defaultEditorSystem") }}</option>
            <option v-for="option in editorTargetOptions" :key="option.value" :value="option.value">{{ option.label }}</option>
          </select>
        </label>
        <p class="wb-settings-help">{{ t("sftpEdit.defaultEditorHint") }}</p>

        <span class="wb-muted">{{ t("sftpEdit.uploadPolicyTitle") }}</span>
        <div class="wb-conflict-options" role="radiogroup" :aria-label="t('sftpEdit.uploadPolicyTitle')">
          <label class="wb-conflict-option">
            <input v-model="editorConfigDraft.uploadPolicy" type="radio" name="files-editor-upload-policy" value="auto" />
            <span>{{ t("sftpEdit.uploadPolicy.auto") }}</span>
          </label>
          <label class="wb-conflict-option">
            <input v-model="editorConfigDraft.uploadPolicy" type="radio" name="files-editor-upload-policy" value="ask" />
            <span>{{ t("sftpEdit.uploadPolicy.ask") }}</span>
          </label>
        </div>
        <p class="wb-settings-help">{{ t("sftpEdit.uploadPolicyHint") }}</p>

        <div class="wb-settings-heading">
          <strong>{{ t("sftpEdit.associationsTitle") }}</strong>
        </div>
        <p v-if="!editorConfigDraft.associations.length" class="wb-settings-help">{{ t("sftpEdit.noAssociations") }}</p>
        <div v-for="(assoc, index) in editorConfigDraft.associations" :key="`${assoc.pattern}-${index}`" class="wb-settings-path-row">
          <span class="wb-mono" style="flex: 0 0 auto">{{ assoc.pattern }}</span>
          <span class="wb-muted" style="flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">→ {{ editorTargetName(associationTargetValue(assoc)) }}</span>
          <button
            class="wb-icon-button wb-icon-neutral"
            type="button"
            :title="t('removeMapping')"
            v-tip="t('removeMapping')"
            @click="removeEditorAssociation(index)"
          ><X /></button>
        </div>
        <div class="wb-settings-path-row">
          <input
            class="wb-mono"
            :value="editorAssociationPatternDraft"
            spellcheck="false"
            style="flex: 0 0 130px"
            :placeholder="t('sftpEdit.associationPatternPlaceholder')"
            :aria-label="t('sftpEdit.associationPatternPlaceholder')"
            @change="onAssociationPatternChange"
          />
          <select :value="editorAssociationTargetDraft" :aria-label="t('sftpEdit.associationTargetPlaceholder')" @change="onAssociationTargetChange">
            <option value="" disabled>{{ t("sftpEdit.associationTargetPlaceholder") }}</option>
            <option v-for="option in editorTargetOptions" :key="option.value" :value="option.value">{{ option.label }}</option>
          </select>
          <button
            class="wb-icon-button wb-icon-neutral"
            type="button"
            :title="t('sftpEdit.addAssociation')"
            v-tip="t('sftpEdit.addAssociation')"
            :disabled="!editorAssociationPatternDraft.trim() || !editorAssociationTargetDraft"
            @click="addEditorAssociation"
          ><Plus /></button>
        </div>
        <p class="wb-settings-help">{{ t("sftpEdit.associationsHint") }}</p>

        <div class="wb-settings-heading">
          <strong>{{ t("sftpEdit.customEditorsTitle") }}</strong>
        </div>
        <p v-if="!editorConfigDraft.customEditors.length" class="wb-settings-help">{{ t("sftpEdit.noCustomEditors") }}</p>
        <div v-for="editor in editorConfigDraft.customEditors" :key="editor.id" class="wb-settings-path-row">
          <span style="flex: 0 0 auto">{{ editor.name }}</span>
          <span class="wb-mono wb-muted" style="flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">{{ editor.command }}</span>
          <button
            class="wb-icon-button wb-icon-neutral"
            type="button"
            :title="t('removeMapping')"
            v-tip="t('removeMapping')"
            @click="removeCustomEditor(editor.id)"
          ><X /></button>
        </div>
      </template>
      <DesktopOnlyCard v-else :t="t" />
    </div>
  </section>
</template>
