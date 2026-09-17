<script setup lang="ts">
// 文本预览/编辑器（与 ssh 插件 sftp 面板同方案）：CodeMirror 6 + 语言包
// 按文件名懒加载，主题色取自宿主 appearance 规范值；editable 切换重建实例。
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { basicSetup } from "codemirror";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { HighlightStyle, LanguageDescription, syntaxHighlighting } from "@codemirror/language";
import { editorLanguages } from "../lib/editorLanguages";
import { tags } from "@lezer/highlight";
// 语法高亮调色板来自 shared 公共层（唯一实现点），暗色为提亮后的 GitHub Dark 系。
import { dbxSyntaxHighlight } from "../../../shared/frontend/editorTheme";

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
  const language = LanguageDescription.matchFilename(editorLanguages, props.fileName);
  const support = language ? await language.load().catch(() => undefined) : undefined;
  return [
    basicSetup,
    // basicSetup 内置 defaultHighlightStyle 是浅底配色，暗色下发暗；此处按宿主
    // 明暗注入 shared 调色板高亮（后声明者优先，内置样式退为 fallback），
    // appearance 变化重建编辑器时随 colorScheme 自然跟随。
    dbxSyntaxHighlight(props.appearance.colorScheme, { HighlightStyle, syntaxHighlighting, tags }) as Extension,
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
