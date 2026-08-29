<script setup lang="ts">
// opendal-custom 编辑器：service 下拉（编译白名单）+ Form/JSON 双模。
// 已知服务按 CUSTOM_SERVICE_SCHEMAS 渲染动态表单（必填标记/placeholder/
// 安全校验），Form ⇄ JSON 双向同步；未知服务仍走纯 JSON。URL 类参数仅
// http/https，host 类参数拒绝环回/私有/保留地址（SSRF 护栏，UI 层）。
// 支持就地 connection/test（走宿主 lifecycle，凭据不落日志）。
import { computed, ref } from "vue";
import {
  CUSTOM_CONFIG_HINTS,
  CUSTOM_SERVICES,
  configFromFormValues,
  formValuesFromConfig,
  schemaForService,
  validateConfigAgainstSchema,
  type CustomFormValue,
  type FieldError,
} from "../lib/opendalServices";
import { callLifecycle, errorMessage } from "../lib/api";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "notice", message: string): void;
  (event: "error", message: string): void;
}>();

const service = ref("fs");
const mode = ref<"form" | "json">("form");
const configDraft = ref("{}");
const formValues = ref<Record<string, CustomFormValue>>({});
const extras = ref<Record<string, unknown>>({});
const switchError = ref("");
const testing = ref(false);
const lastResult = ref<"" | "ok" | "failed">("");

const services = computed(() => Array.from(CUSTOM_SERVICES).sort());
const schema = computed(() => schemaForService(service.value));
/** 未知服务拿不到 schema → 只保留 JSON 模式。 */
const formAvailable = computed(() => schema.value !== undefined);
const hint = computed(() => CUSTOM_CONFIG_HINTS[service.value] ?? "{}");

const FIELD_ERROR_KEY: Record<FieldError, string> = {
  required: "customFieldRequiredError",
  url: "customFieldUrl",
  host: "customFieldHost",
  number: "customFieldNumber",
};

/** 表单模式下的实时配置（schema 字段 + extras 合并）。 */
const formConfig = computed(() => configFromFormValues(schema.value ?? [], formValues.value, extras.value));
const formErrors = computed<Record<string, FieldError>>(() =>
  schema.value ? validateConfigAgainstSchema(schema.value, formConfig.value) : {},
);

function parseDraft(): { ok: true; config: Record<string, unknown> } | { ok: false; error: string } {
  const raw = configDraft.value.trim() || "{}";
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return { ok: false, error: "config must be a JSON object" };
    }
    return { ok: true, config: parsed as Record<string, unknown> };
  } catch (cause) {
    return { ok: false, error: errorMessage(cause) };
  }
}

function hydrateForm(config: Record<string, unknown>) {
  const hydration = formValuesFromConfig(schema.value ?? [], config);
  formValues.value = hydration.values;
  extras.value = hydration.extras;
}

function syncDraftFromForm() {
  configDraft.value = JSON.stringify(formConfig.value, null, 2);
}

function onServiceChange() {
  switchError.value = "";
  lastResult.value = "";
  if (!configDraft.value.trim() || configDraft.value.trim() === "{}") {
    configDraft.value = hint.value;
  }
  const parsed = parseDraft();
  if (formAvailable.value) {
    if (parsed.ok) hydrateForm(parsed.config);
    mode.value = "form";
  } else {
    mode.value = "json";
  }
}

function switchMode(target: "form" | "json") {
  if (target === mode.value) return;
  switchError.value = "";
  if (target === "form") {
    if (!formAvailable.value) return;
    const parsed = parseDraft();
    // JSON 非法时禁止切到表单并提示（保持留在 JSON 模式修正）。
    if (!parsed.ok) {
      switchError.value = props.t("customJsonInvalidSwitch", { error: parsed.error });
      return;
    }
    hydrateForm(parsed.config);
    mode.value = "form";
    return;
  }
  syncDraftFromForm();
  mode.value = "json";
}

function setFieldValue(key: string, value: CustomFormValue) {
  formValues.value = { ...formValues.value, [key]: value };
  // 表单改动实时序列化进 JSON（双向同步的 form → JSON 方向）。
  syncDraftFromForm();
}

function errorText(key: string): string {
  const code = formErrors.value[key];
  return code ? props.t(FIELD_ERROR_KEY[code]) : "";
}

async function test() {
  let config: Record<string, unknown>;
  if (mode.value === "form") {
    const errorKeys = Object.keys(formErrors.value);
    if (errorKeys.length) {
      emit("error", errorText(errorKeys[0]));
      return;
    }
    config = formConfig.value;
  } else {
    const parsed = parseDraft();
    if (!parsed.ok) {
      emit("error", props.t("customConfigInvalid", { error: parsed.error }));
      return;
    }
    config = parsed.config;
  }
  testing.value = true;
  lastResult.value = "";
  try {
    const external = {
      protocol: "opendal-custom",
      service: service.value,
      config,
    };
    // 与宿主表单一致：external_config 直入 Builder，凭据走 secret binding。
    await callLifecycle("connection/test", {
      provider: { id: "io.dbx.files.connection", databaseType: "storage" },
      connection: { id: `ui-probe-${Date.now()}`, name: "probe", db_type: "storage", external_config: external },
      runtime: {},
    });
    lastResult.value = "ok";
    emit("notice", props.t("customConfigTestOk"));
  } catch (cause) {
    lastResult.value = "failed";
    emit("error", props.t("customConfigTestFailed", { error: errorMessage(cause) }));
  } finally {
    testing.value = false;
  }
}
</script>

<template>
  <div style="display: flex; flex-direction: column; gap: 8px">
    <p class="wb-muted" style="margin: 0; font-size: 11px">{{ t("customConfigHint") }}</p>
    <label style="display: flex; flex-direction: column; gap: 4px">
      <span>{{ t("customConfigService") }}</span>
      <select v-model="service" @change="onServiceChange">
        <option v-for="item in services" :key="item" :value="item">{{ item }}</option>
      </select>
    </label>

    <!-- Form/JSON 双 Tab：未知服务隐藏 Form Tab -->
    <div v-if="formAvailable" class="wb-pane-tabs" role="tablist">
      <button type="button" :class="{ 'is-active': mode === 'form' }" @click="switchMode('form')">{{ t("customFormTab") }}</button>
      <button type="button" :class="{ 'is-active': mode === 'json' }" @click="switchMode('json')">{{ t("customJsonTab") }}</button>
    </div>
    <p v-if="switchError" class="wb-icon-danger" style="margin: 0; font-size: 11px">{{ switchError }}</p>

    <!-- Form 模式：按 schema 渲染动态控件 -->
    <template v-if="formAvailable && mode === 'form'">
      <p v-if="!schema!.length" class="wb-muted" style="margin: 0; font-size: 11px">{{ t("customFormNoParams") }}</p>
      <div
        v-for="spec in schema"
        :key="spec.key"
        style="display: flex; flex-direction: column; gap: 4px"
      >
        <label v-if="spec.type === 'boolean'" style="display: flex; align-items: center; gap: 6px">
          <input
            type="checkbox"
            :checked="Boolean(formValues[spec.key])"
            @change="setFieldValue(spec.key, ($event.target as HTMLInputElement).checked)"
          />
          <span><code class="wb-mono">{{ spec.key }}</code></span>
        </label>
        <template v-else>
          <span>
            <code class="wb-mono">{{ spec.key }}</code>
            <span v-if="spec.required" class="wb-icon-danger" :title="t('customFieldRequired')">&nbsp;*</span>
          </span>
          <select
            v-if="spec.type === 'select'"
            :value="String(formValues[spec.key] ?? '')"
            @change="setFieldValue(spec.key, ($event.target as HTMLSelectElement).value)"
          >
            <option value=""></option>
            <option v-for="option in spec.options" :key="option" :value="option">{{ option }}</option>
          </select>
          <input
            v-else-if="spec.type === 'password'"
            type="password"
            :value="String(formValues[spec.key] ?? '')"
            :placeholder="spec.placeholder"
            spellcheck="false"
            autocomplete="off"
            @input="setFieldValue(spec.key, ($event.target as HTMLInputElement).value)"
          />
          <input
            v-else
            :type="spec.type === 'number' ? 'number' : 'text'"
            :value="String(formValues[spec.key] ?? '')"
            :placeholder="spec.placeholder"
            spellcheck="false"
            @input="setFieldValue(spec.key, ($event.target as HTMLInputElement).value)"
          />
          <span v-if="errorText(spec.key)" class="wb-icon-danger" style="font-size: 11px">{{ errorText(spec.key) }}</span>
        </template>
      </div>
    </template>

    <!-- JSON 模式：未知服务的唯一入口；已知服务与表单双向同步 -->
    <label v-if="!formAvailable || mode === 'json'" style="display: flex; flex-direction: column; gap: 4px">
      <span>{{ t("customConfigConfig") }}</span>
      <textarea v-model="configDraft" class="wb-mono" spellcheck="false" />
    </label>

    <div style="display: flex; align-items: center; gap: 8px">
      <button type="button" class="wb-toolbar-button" :disabled="testing" @click="test">
        {{ t("customConfigTest") }}
      </button>
      <span v-if="lastResult === 'ok'" class="wb-icon-emerald">{{ t("customConfigTestOk") }}</span>
      <span v-else-if="lastResult === 'failed'" class="wb-icon-danger">{{ t("customConfigTestFailed", { error: "" }) }}</span>
    </div>
  </div>
</template>
