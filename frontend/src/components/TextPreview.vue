<script setup lang="ts">
// 文本预览/编辑器（与 ssh 插件 sftp 面板同方案）：CodeMirror 6 + 语言包
// 按文件名懒加载，主题色取自宿主 appearance 规范值；editable 切换重建实例。
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { basicSetup } from "codemirror";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { LanguageDescription } from "@codemirror/language";
import { languages } from "@codemirror/language-data";

const props = defineProps<{
  text: string;
  fileName: string;
  appearance: DbxPluginAppearance;
  editable?: boolean;
}>();

const emit = defineEmits<{
  change: [text: string];
}>();

const host = ref<HTMLElement>();
let view: EditorView | undefined;
let generation = 0;

function previewTheme() {
  const colors = props.appearance.colors;
  return EditorView.theme({
    "&": { height: "100%", backgroundColor: colors.background, color: colors.foreground },
    ".cm-scroller": {
      overflow: "auto",
      fontFamily: props.appearance.terminal.fontFamily,
      fontSize: `${props.appearance.terminal.fontSize}px`,
    },
    ".cm-gutters": { backgroundColor: colors.muted, color: colors.mutedForeground, borderRightColor: colors.border },
    ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: colors.accent },
    ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "#5f7aa855" },
  }, { dark: props.appearance.colorScheme === "dark" });
}

async function extensions() {
  const language = LanguageDescription.matchFilename(languages, props.fileName);
  const support = language ? await language.load().catch(() => undefined) : undefined;
  return [
    basicSetup,
    EditorState.readOnly.of(!props.editable),
    EditorView.editable.of(props.editable === true),
    EditorView.lineWrapping,
    previewTheme(),
    EditorView.updateListener.of((update) => {
      if (update.docChanged) emit("change", update.state.doc.toString());
    }),
    ...(support ? [support] : []),
  ];
}

async function createEditor() {
  const current = ++generation;
  const configured = await extensions();
  if (!host.value || current !== generation) return;
  // Seed the new editor with the live document so edits survive theme or
  // editable-mode re-creations; fall back to the incoming text on first mount.
  const doc = view?.state.doc.toString() ?? props.text;
  view?.destroy();
  view = new EditorView({
    parent: host.value,
    state: EditorState.create({ doc, extensions: configured }),
  });
  // P1-4：进入编辑态（editable 重建实例）后主动聚焦编辑器——
  // extensions() 异步装载语言包，外部 nextTick 后 focus 可能早于实例就绪，
  // 以这里为权威时机；主题切换等重建时保持焦点不丢。
  if (props.editable) view.focus();
}

/** 供父组件（PreviewPane）在进入编辑态后主动聚焦。 */
function focus() {
  view?.focus();
}

onMounted(createEditor);
watch(() => [props.fileName, props.appearance, props.editable] as const, createEditor, { deep: true });
watch(() => props.text, (text) => {
  if (!view || text === view.state.doc.toString()) return;
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
});
onBeforeUnmount(() => {
  generation += 1;
  view?.destroy();
});
defineExpose({ focus });
</script>

<template>
  <div ref="host" class="preview-editor" />
</template>
