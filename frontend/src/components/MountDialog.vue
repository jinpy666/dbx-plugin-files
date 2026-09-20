<script setup lang="ts">
// 挂载到本机对话框（独立顶层弹窗，z-index 高于设置/预览层）：内嵌本机目录
// 浏览器（files/list + __local__ 保留连接）直接选目录后确认挂载——不依赖宿主
// pickDirectory（Host API 1.x 未提供该能力）。挂载位置留空 = sidecar 默认
// （~/dbx-files-mounts/<连接>）；所选目录不存在时由后端挂载时自动创建。
import { computed, onMounted, ref } from "vue";
import { ArrowUp, Folder, HardDrive, X } from "@lucide/vue";
import { normalizeEntries, type FileEntry } from "../lib/api";
import { isNotFoundMessage } from "../lib/friendlyError";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "close"): void;
  /** mountPoint 为空串 = 使用 sidecar 默认位置。 */
  (event: "confirm", mountPoint: string): void;
}>();

const t = (key: string, values?: Record<string, string | number>) => props.t(key, values);

/** 输入框（编辑期真源）：空 = 默认位置。 */
const inputPath = ref("");
/** 当前浏览的本机目录（仅导航；与输入框在浏览动作时同步）。 */
const browsePath = ref("");
const browsing = ref(false);
const browseError = ref("");
/** 目录不存在不算错误：挂载时会自动创建，仍允许确认。 */
const notFound = ref(false);
const dirs = ref<string[]>([]);

onMounted(() => {
  void startFromHome();
});

/** 浏览起点 = 本机 home（quickPaths 的 home 项）；拿不到回退根目录。 */
async function startFromHome() {
  try {
    const result = await window.dbxPlugin.invoke<{ paths: Array<{ key?: string; path: string }> }>(
      "files/quickPaths",
      { connectionId: "__local__" },
    );
    const home = (result.paths ?? []).find((item) => item.key === "home");
    await browse(home?.path ?? "/");
  } catch {
    await browse("/");
  }
}

async function browse(target: string) {
  const clean = target.trim();
  if (!clean) return;
  browsing.value = true;
  browseError.value = "";
  notFound.value = false;
  try {
    const result = await window.dbxPlugin.invoke<{ entries?: FileEntry[] }>(
      "files/list",
      { connectionId: "__local__", path: clean },
    );
    // 隐藏目录（. 开头）不进列表——home 下几十个 dot 目录会把浏览体验
    // 淹没；需要时仍可手输路径进入（挂载点常在隐藏位置如 ~/.config）。
    dirs.value = normalizeEntries(result.entries ?? [])
      .filter((entry) => entry.kind === "directory" && !entry.name.startsWith("."))
      .map((entry) => entry.name)
      .sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
    browsePath.value = clean;
    inputPath.value = clean;
  } catch (cause) {
    if (isNotFoundMessage(cause instanceof Error ? cause.message : String(cause))) {
      notFound.value = true;
      browsePath.value = clean;
      inputPath.value = clean;
      dirs.value = [];
    } else {
      browseError.value = cause instanceof Error ? cause.message : String(cause);
    }
  } finally {
    browsing.value = false;
  }
}

/** unix 风格面包屑（__local__ 的路径形态；反斜杠兜底 Windows 形态）。 */
const crumbs = computed(() => {
  const parts = browsePath.value.split(/[\\/]/).filter(Boolean);
  const list: Array<{ name: string; path: string }> = [{ name: "/", path: "/" }];
  let acc = "";
  for (const part of parts) {
    acc = browsePath.value.includes("\\") ? `${acc}${acc ? "\\" : ""}${part}` : `${acc}/${part}`;
    list.push({ name: part, path: acc });
  }
  return list;
});

const parentPath = computed(() => {
  const current = browsePath.value;
  if (current.includes("\\")) {
    const index = current.lastIndexOf("\\");
    return index > 0 ? current.slice(0, index) : current;
  }
  const trimmed = current.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index > 0 ? trimmed.slice(0, index) : "/";
});

function enter(name: string) {
  const base = browsePath.value.replace(/[\\/]+$/, "");
  const separator = browsePath.value.includes("\\") ? "\\" : "/";
  void browse(base.endsWith(":") ? `${base}${separator}${name}` : `${base}/${name}`);
}
</script>

<template>
  <div class="wb-mount-backdrop" role="dialog" aria-modal="true" :aria-label="t('mountToLocal')" @click.self="emit('close')">
    <div class="wb-mount-dialog">
      <header>
        <strong><HardDrive class="wb-mount-title-icon" /> {{ t("mountToLocal") }}</strong>
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('close')" @click="emit('close')"><X /></button>
      </header>
      <p class="wb-mount-hint">{{ t("mountDialogBody") }}</p>
      <!-- 挂载位置：手输 + 内嵌本机目录浏览（面包屑 / 上级 / 子目录）。 -->
      <label class="wb-mount-field">
        <span>{{ t("mountPointLabel") }}</span>
        <input
          v-model="inputPath"
          class="wb-mono"
          spellcheck="false"
          :placeholder="t('mountPointPlaceholder')"
          @keydown.enter.prevent="browse(inputPath)"
        />
      </label>
      <div class="wb-mount-browse-toolbar">
        <button class="wb-icon-button wb-icon-neutral" v-tip="t('up')" :disabled="browsing" @click="browse(parentPath)"><ArrowUp /></button>
        <nav class="wb-mount-crumbs" aria-label="breadcrumb">
          <template v-for="(crumb, index) in crumbs" :key="crumb.path">
            <span v-if="index" class="wb-mount-crumb-sep">/</span>
            <button type="button" class="wb-mount-crumb" @click="browse(crumb.path)">{{ crumb.name }}</button>
          </template>
        </nav>
        <button class="wb-link-button" type="button" @click="inputPath = ''; browsePath = ''">{{ t("mountUseDefault") }}</button>
      </div>
      <div class="wb-mount-list" :aria-busy="browsing">
        <span v-if="browsing" class="wb-muted">{{ t("loading") }}</span>
        <template v-else-if="notFound">
          <span class="wb-muted">{{ t("mountBrowseCreateHint") }}</span>
        </template>
        <span v-else-if="browseError" class="wb-mount-error">{{ browseError }}</span>
        <span v-else-if="!dirs.length" class="wb-muted">{{ t("mountBrowseEmpty") }}</span>
        <template v-else>
          <button
            v-for="name in dirs"
            :key="name"
            type="button"
            class="wb-mount-dir"
            @click="enter(name)"
          ><Folder class="wb-mount-dir-icon" /> <span class="wb-mount-dir-name">{{ name }}</span></button>
        </template>
      </div>
      <footer>
        <span class="wb-muted wb-mount-foot-hint">{{ t("mountPointHint") }}</span>
        <span class="wb-mount-foot-actions">
          <button class="wb-dialog-cancel" type="button" @click="emit('close')">{{ t("cancel") }}</button>
          <button class="wb-dialog-primary" type="button" @click="emit('confirm', inputPath.trim())">{{ t("mountConfirm") }}</button>
        </span>
      </footer>
    </div>
  </div>
</template>
