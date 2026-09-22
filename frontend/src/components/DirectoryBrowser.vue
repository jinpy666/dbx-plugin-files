<script setup lang="ts">
// 目录浏览器（挂载/设置下载目录在 __local__ 浏览；SyncDialog 传 connectionId
// 在当前连接上选路径）：files/list 逐级浏览；面包屑可跳级，上级按钮回父目录。
// 选择结果通过 navigate 上抛，由父层决定填入哪个输入框。initialPath 给出时
// 直接从该目录起步（跳过 home 探测，避免异步回写覆盖父层指定的起点）。
import { computed, onMounted, ref } from "vue";
import { ArrowUp, Folder } from "@lucide/vue";
import { normalizeEntries, type FileEntry } from "../lib/api";
import { isNotFoundMessage } from "../lib/friendlyError";

const props = defineProps<{
  t: (key: string, values?: Record<string, string | number>) => string;
  /** 浏览目标连接；缺省 = 本机 __local__（既有挂载/设置语义）。 */
  connectionId?: string;
  /** 挂载后首个浏览目录；缺省 = 连接 quickPaths 的 home（拿不到回退根）。 */
  initialPath?: string;
  /** 目录不存在时的提示文案 key（挂载场景默认「确认时自动创建」语义）。 */
  missingHintKey?: string;
}>();

const emit = defineEmits<{
  (event: "navigate", path: string): void;
}>();

/** 当前浏览的本机目录（与父层输入框在导航时同步）。 */
const browsePath = ref("");
const browsing = ref(false);
const browseError = ref("");
/** 目录不存在不算错误：仍允许确认（挂载/新建场景会自动创建）。 */
const notFound = ref(false);
const dirs = ref<string[]>([]);

onMounted(() => {
  if (props.initialPath) void browse(props.initialPath, true);
  else void startFromHome();
});

/** 浏览起点 = 连接 quickPaths 的 home 项；拿不到回退根目录。 */
async function startFromHome() {
  try {
    const result = await window.dbxPlugin.invoke<{ paths: Array<{ key?: string; path: string }> }>(
      "files/quickPaths",
      { connectionId: props.connectionId ?? "__local__" },
    );
    const home = (result.paths ?? []).find((item) => item.key === "home");
    await browse(home?.path ?? "/", true);
  } catch {
    await browse("/", true);
  }
}

async function browse(target: string, silent = false) {
  const clean = target.trim();
  if (!clean) return;
  browsing.value = true;
  browseError.value = "";
  notFound.value = false;
  try {
    const result = await window.dbxPlugin.invoke<{ entries?: FileEntry[] }>(
      "files/list",
      { connectionId: props.connectionId ?? "__local__", path: clean },
    );
    // 隐藏目录（. 开头）不进列表——home 下几十个 dot 目录会淹没浏览体验。
    dirs.value = normalizeEntries(result.entries ?? [])
      .filter((entry) => entry.kind === "directory" && !entry.name.startsWith("."))
      .map((entry) => entry.name)
      .sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
    browsePath.value = clean;
    // 初始定位（home）不回抛：父层「留空 = 默认位置」的契约不能被启动加载打破。
    if (!silent) emit("navigate", clean);
  } catch (cause) {
    if (isNotFoundMessage(cause instanceof Error ? cause.message : String(cause))) {
      notFound.value = true;
      browsePath.value = clean;
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

defineExpose({ browse });
</script>

<template>
  <div>
    <div class="wb-mount-browse-toolbar">
      <button class="wb-icon-button wb-icon-neutral" v-tip="t('up')" :disabled="browsing" @click="browse(parentPath)"><ArrowUp /></button>
      <nav class="wb-mount-crumbs" aria-label="breadcrumb">
        <template v-for="(crumb, index) in crumbs" :key="crumb.path">
          <!-- crumbs[0] 就是根 "/"，自带斜杠；只为后续层级加分隔，避免 "/ / Users"。 -->
          <span v-if="index > 1" class="wb-mount-crumb-sep">/</span>
          <button type="button" class="wb-mount-crumb" @click="browse(crumb.path)">{{ crumb.name }}</button>
        </template>
      </nav>
    </div>
    <div class="wb-mount-list" :aria-busy="browsing">
      <span v-if="browsing" class="wb-muted">{{ t("loading") }}</span>
      <span v-else-if="notFound" class="wb-muted">{{ t(props.missingHintKey ?? "mountBrowseCreateHint") }}</span>
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
  </div>
</template>
