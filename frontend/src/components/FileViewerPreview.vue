<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import { FileViewer, type FileViewerOptions } from "@file-viewer/vue3";
import allRenderers from "@file-viewer/preset-all";

export type FileViewerPreviewSource = Blob | File | string;

type LifecycleContext = unknown;

const props = withDefaults(
  defineProps<{
    source: FileViewerPreviewSource;
    fileName?: string;
    mime?: string;
    options?: FileViewerOptions;
    class?: string;
    height?: string | number;
  }>(),
  {
    fileName: undefined,
    mime: undefined,
    options: undefined,
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

const viewerOptions = computed<FileViewerOptions>(() => ({
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
