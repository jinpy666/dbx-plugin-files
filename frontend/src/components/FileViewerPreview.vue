<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import { FileViewer, type FileViewerOptions } from "@file-viewer/vue3";
import allRenderers from "@file-viewer/preset-all";
import "@file-viewer/vue3/dist/file-viewer3.css";

export type FileViewerPreviewSource = Blob | File | string;

type LifecycleContext = unknown;

const props = withDefaults(
  defineProps<{
    source: FileViewerPreviewSource;
    fileName?: string;
    mime?: string;
    options?: FileViewerOptions;
    /** 宿主外观：viewer 主题跟随插件配色方案（light/dark）。 */
    appearance?: DbxPluginAppearance;
    class?: string;
    height?: string | number;
  }>(),
  {
    fileName: undefined,
    mime: undefined,
    options: undefined,
    appearance: undefined,
    class: undefined,
    height: undefined,
  },
);

const emit = defineEmits<{
  (event: "load-start", context: LifecycleContext): void;
  (event: "load", context: LifecycleContext): void;
  (event: "unload-start", context: LifecycleContext): void;
  (event: "unload", context: LifecycleContext): void;
  (event: "error", error: unknown): void;
}>();

const file = ref<File>();
const url = ref("");
let ownedUrl: string | undefined;

function revokeOwnedUrl() {
  if (!ownedUrl) return;
  URL.revokeObjectURL(ownedUrl);
  ownedUrl = undefined;
}

function updateSource(source: FileViewerPreviewSource) {
  revokeOwnedUrl();
  file.value = undefined;

  if (typeof source === "string") {
    url.value = source;
    return;
  }

  const fileName = props.fileName || (source instanceof File ? source.name : "file");
  const type = props.mime || source.type || undefined;
  const wrapped = new File([source], fileName, type ? { type } : undefined);
  const objectUrl = URL.createObjectURL(wrapped);
  file.value = wrapped;
  url.value = objectUrl;
  ownedUrl = objectUrl;
}

watch(
  [() => props.source, () => props.fileName, () => props.mime],
  ([source]) => updateSource(source),
  { immediate: true },
);

onUnmounted(revokeOwnedUrl);

// 预览外壳（搜索/缩放/下载/打印工具栏）由宿主 PreviewPane 头部承担，默认关闭；
// 主题跟随插件外观配色，surfaceBackground 透明让内容融入预览面板（消除自带
// 底色与边框感）。调用方仍可通过 props.options 覆盖任意默认值。
const viewerOptions = computed<FileViewerOptions>(() => ({
  toolbar: false,
  search: { enabled: false },
  theme: props.appearance?.colorScheme === "light" ? "light" : "dark",
  ui: { surfaceBackground: "transparent" },
  ...(props.options ?? {}),
  rendererMode: props.options?.rendererMode ?? "replace",
  preset: allRenderers,
}));

const viewerSize = computed(() => {
  if (props.height === undefined) return undefined;
  return typeof props.height === "number" ? `${props.height}px` : props.height;
});

function emitError(error: unknown) {
  emit("error", error);
}
</script>

<template>
  <FileViewer
    :file="file"
    :url="url || undefined"
    :name="fileName"
    :filename="fileName"
    :type="mime"
    :size="file?.size"
    :options="viewerOptions"
    :class="props.class"
    :style="viewerSize ? { height: viewerSize } : undefined"
    @load-start="emit('load-start', $event)"
    @load-complete="emit('load', $event)"
    @unload-start="emit('unload-start', $event)"
    @unload-complete="emit('unload', $event)"
    @error="emitError"
    @unsupported="emitError"
  />
</template>
