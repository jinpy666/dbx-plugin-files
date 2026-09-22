<script setup lang="ts">
// 自定义 rclone 后端编辑器（rclone-custom 透传的 test 探针）：service 可输入
// （datalist 建议 rclone 后端全集，新版本新增的类型手输可达），参数以 JSON
// 原样透传（键名见 rclone 文档）。支持就地 connection/test（走宿主 lifecycle，
// 凭据不落日志）。真正建连接走宿主的连接表单。
import { computed, ref } from "vue";
import { CUSTOM_CONFIG_HINTS, customServiceSuggestions } from "../lib/rcloneServices";
import { callLifecycle, errorMessage } from "../lib/api";
import type { I18nText } from "../lib/i18n";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

// R5-P2-7 同类收尾：notice/error 改发 key + 参数形态（I18nText），由 App 侧
// 横幅渲染时经 locale 求值——提示/横幅存活期间切 locale 即时跟随，不再
// 固化 emit 瞬间的译文。字符串形态仍兼容（App showError/showNotice 双收）。
const emit = defineEmits<{
  (event: "notice", message: string | I18nText): void;
  (event: "error", cause: string | I18nText): void;
}>();

const service = ref("");
const configDraft = ref("{}");
const testing = ref(false);
const lastResult = ref<"" | "ok" | "failed">("");
// 最近一次自动播种的示例：草稿仍是它（用户未编辑）时，换后端跟随重新播种。
let lastSeed = "";

const services = computed(() => customServiceSuggestions());

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

function onServiceChange() {
  lastResult.value = "";
  const hint = CUSTOM_CONFIG_HINTS[service.value.trim()];
  if (!hint) return;
  const draft = configDraft.value.trim();
  if (!draft || draft === "{}" || draft === lastSeed) {
    configDraft.value = hint;
    lastSeed = hint;
  }
}

async function test() {
  const parsed = parseDraft();
  if (!parsed.ok) {
    emit("error", { key: "customConfigInvalid", values: { error: parsed.error } });
    return;
  }
  testing.value = true;
  lastResult.value = "";
  try {
    const external = {
      protocol: "rclone-custom",
      service: service.value.trim(),
      config: parsed.config,
    };
    // 与宿主表单一致：external_config 直入 rclone config/create，凭据不落日志。
    await callLifecycle("connection/test", {
      provider: { id: "io.dbx.files.connection", databaseType: "storage" },
      connection: { id: `ui-probe-${Date.now()}`, name: "probe", db_type: "storage", external_config: external },
      runtime: {},
    });
    lastResult.value = "ok";
    emit("notice", { key: "customConfigTestOk" });
  } catch (cause) {
    lastResult.value = "failed";
    emit("error", { key: "customConfigTestFailed", values: { error: errorMessage(cause) } });
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
      <!-- rclone 引擎的透传可达任意 rclone 后端：service 可输入（datalist
           建议），后端在使用时按实际 rclone 校验。 -->
      <input
        id="custom-service-input"
        v-model="service"
        list="custom-service-options"
        autocomplete="off"
        spellcheck="false"
        @change="onServiceChange"
      />
      <datalist id="custom-service-options">
        <option v-for="item in services" :key="item" :value="item"></option>
      </datalist>
    </label>

    <label style="display: flex; flex-direction: column; gap: 4px">
      <span>{{ t("customConfigConfig") }}</span>
      <textarea v-model="configDraft" class="wb-mono" spellcheck="false" />
    </label>

    <div style="display: flex; align-items: center; gap: 8px">
      <button type="button" class="wb-action-button" :disabled="testing" @click="test">
        {{ t("customConfigTest") }}
      </button>
      <span v-if="lastResult === 'ok'" class="wb-icon-emerald">{{ t("customConfigTestOk") }}</span>
      <span v-else-if="lastResult === 'failed'" class="wb-icon-danger">{{ t("customConfigTestFailed", { error: "" }) }}</span>
    </div>
  </div>
</template>
