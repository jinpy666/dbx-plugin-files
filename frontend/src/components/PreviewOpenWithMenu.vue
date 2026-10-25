<script setup lang="ts">
// 预览头部「打开方式」下拉（与 SSH 插件预览同形）：系统默认 + sidecar 探测到的
// 可用编辑器 + 用户自定义编辑器，最后进入自定义命令弹窗。选中即交给 App 的
// openInExternalEditor 走既有 remote-edit 下载/监听/回传链路，不新增执行原语。
import { computed, nextTick, onUnmounted, ref, watch } from "vue";
import { Check, ExternalLink, Pencil } from "@lucide/vue";
import { resolveEditorForFile, type EditorConfig, type KnownEditor, type ResolvedEditor } from "../lib/editorRules";
import { vTip } from "../lib/tooltip";

const props = defineProps<{
  fileName: string;
  editors: KnownEditor[];
  editorConfig: EditorConfig;
  /** 本机落盘能力：web/docker 形态下没有本机副本可打开。 */
  canOpen: boolean;
  /** 不可用时的悬浮说明。 */
  disabledHint?: string;
  t: (key: string, values?: Record<string, string | number>) => string;
}>();

const emit = defineEmits<{
  (event: "open", editor: ResolvedEditor): void;
  (event: "custom-command"): void;
  (event: "request-editors"): void;
}>();

const open = ref(false);
const rootEl = ref<HTMLElement>();
const triggerEl = ref<HTMLButtonElement>();
const menuEl = ref<HTMLElement>();

const availableEditors = computed(() => props.editors.filter((editor) => editor.available));
/** 当前文件按关联/默认解析到的编辑器 id（菜单打勾标识；system 为空串）。 */
const mappedEditorId = computed(() => {
  const resolved = resolveEditorForFile(props.fileName, props.editorConfig, props.editors);
  return resolved.kind === "system" ? "" : resolved.editor.id;
});

function setOpen(next: boolean) {
  open.value = next;
  // 展开时惰性拉取编辑器目录（与右键子菜单同一时机，避免启动即探测本机）。
  if (next) emit("request-editors");
}

function toggle() {
  if (!props.canOpen) return;
  setOpen(!open.value);
}

function choose(editor: ResolvedEditor) {
  setOpen(false);
  emit("open", editor);
}

function chooseCustomCommand() {
  setOpen(false);
  emit("custom-command");
}

// 外点关闭：pointerdown 捕获，避免点击落到遮罩/列表后才关（与右键菜单同语义）。
function onDocumentPointerDown(event: PointerEvent) {
  if (!open.value) return;
  const target = event.target as Node | null;
  if (target && (rootEl.value?.contains(target) || menuEl.value?.contains(target))) return;
  setOpen(false);
}

function onDocumentKeydown(event: KeyboardEvent) {
  if (!open.value || event.key !== "Escape") return;
  // 预览层自身的 Esc 关闭链在 bubble 阶段；菜单开着时先只收菜单。
  event.preventDefault();
  event.stopPropagation();
  setOpen(false);
  triggerEl.value?.focus();
}

function onMenuKeydown(event: KeyboardEvent) {
  if (!open.value) return;
  if (event.key === "Escape") {
    onDocumentKeydown(event);
    return;
  }
  const keys = ["ArrowDown", "ArrowUp", "Home", "End"];
  if (!keys.includes(event.key)) return;
  event.preventDefault();
  const items = Array.from(menuEl.value?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? []);
  if (!items.length) return;
  const current = items.indexOf(document.activeElement as HTMLButtonElement);
  const last = items.length - 1;
  let next = 0;
  if (event.key === "End") next = last;
  else if (event.key === "Home") next = 0;
  else if (event.key === "ArrowUp") next = current <= 0 ? last : current - 1;
  else next = current < 0 || current >= last ? 0 : current + 1;
  items[next]?.focus();
}

watch(open, async (isOpen) => {
  if (isOpen) {
    document.addEventListener("pointerdown", onDocumentPointerDown, true);
    document.addEventListener("keydown", onDocumentKeydown, true);
    await nextTick();
    menuEl.value?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  } else {
    document.removeEventListener("pointerdown", onDocumentPointerDown, true);
    document.removeEventListener("keydown", onDocumentKeydown, true);
  }
});

// 预览切文件 / 本机能力消失时收起，避免菜单继续指向旧文件。
watch(
  () => [props.fileName, props.canOpen],
  () => setOpen(false),
);

onUnmounted(() => {
  document.removeEventListener("pointerdown", onDocumentPointerDown, true);
  document.removeEventListener("keydown", onDocumentKeydown, true);
});
</script>

<template>
  <div ref="rootEl" class="wb-open-with">
    <button
      ref="triggerEl"
      type="button"
      class="wb-icon-button wb-icon-neutral"
      :disabled="!canOpen"
      :aria-label="t('openWithMenu')"
      aria-haspopup="menu"
      :aria-expanded="open"
      data-test="preview-open-with"
      v-tip="canOpen ? t('openWithMenu') : disabledHint"
      @click="toggle"
    >
      <ExternalLink />
    </button>
    <div
      v-if="open"
      ref="menuEl"
      class="wb-context-menu wb-open-with-menu"
      role="menu"
      data-test="preview-open-with-menu"
      @click.stop
      @keydown="onMenuKeydown"
    >
      <button type="button" role="menuitem" @click="choose({ kind: 'system' })">
        <span class="wb-open-with-label">{{ t("sftpEdit.systemDefault") }}</span>
        <Check v-if="!mappedEditorId" class="wb-menu-check" />
      </button>
      <template v-if="availableEditors.length">
        <hr />
        <button
          v-for="editor in availableEditors"
          :key="`known-${editor.id}`"
          type="button"
          role="menuitem"
          :title="editor.launch.kind === 'exe' ? editor.launch.exe : editor.launch.appId"
          @click="choose({ kind: 'known', editor })"
        >
          <span class="wb-open-with-label">{{ editor.name }}</span>
          <Check v-if="mappedEditorId === editor.id" class="wb-menu-check" />
        </button>
      </template>
      <template v-if="editorConfig.customEditors.length">
        <hr />
        <button
          v-for="editor in editorConfig.customEditors"
          :key="`custom-${editor.id}`"
          type="button"
          role="menuitem"
          :title="editor.command"
          @click="choose({ kind: 'custom', editor })"
        >
          <span class="wb-open-with-label">{{ editor.name }}</span>
          <Check v-if="mappedEditorId === editor.id" class="wb-menu-check" />
        </button>
      </template>
      <hr />
      <button type="button" role="menuitem" @click="chooseCustomCommand">
        <Pencil />
        <span class="wb-open-with-label">{{ t("sftpEdit.customCommand") }}</span>
      </button>
    </div>
  </div>
</template>
