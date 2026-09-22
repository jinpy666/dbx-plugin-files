<script setup lang="ts">
// Markdown 渲染视图（零依赖轻量白名单子集，替代未装配的 mermaid-markdown capability）：
//   块级：标题 #~######、围栏代码块、无序/有序列表、引用、水平线、段落；
//   行内：粗体/斜体/行内代码、链接（scheme 白名单）。
// 安全模型：全文先做 HTML 转义再匹配语法，未识别语法保持转义原文；
// 解析抛异常时 emit render-error，由 PreviewPane 回退 CodeMirror 源码视图。
import { computed, watch } from "vue";

const props = defineProps<{ text: string }>();
const emit = defineEmits<{ (event: "render-error"): void }>();

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// 行内代码占位符：先摘出 code span（内部不再二次处理），段末还原。
// 输入文本已剔除 NUL，占位符不会与正文冲突。
const CODE_SLOT = "\u0000";

/** 行内渲染入口：先 HTML 转义（安全模型第一步），再匹配行内语法。
 * 块级结构检测在原文上进行（`>` 等标记不受实体化影响），叶子内容一律
 * 经此函数输出，保证任何未识别语法都以转义原文落地。 */
function renderInline(raw: string): string {
  const escaped = escapeHtml(raw);
  const codeSpans: string[] = [];
  let working = escaped.replace(/`([^`]+)`/g, (_match, code: string) => {
    codeSpans.push(`<code class="wb-md-code-inline">${code}</code>`);
    return `${CODE_SLOT}${codeSpans.length - 1}${CODE_SLOT}`;
  });
  // 链接 [text](url)：href 已随全文转义（引号为 &quot;，属性注入不可行），
  // scheme 白名单拦截 javascript:/data:/vbscript: 等注入面；非法 scheme
  // 原样保留为可读文本（回退原文约定）。
  working = working.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (match, label: string, href: string) => {
    const scheme = (/^([a-z][a-z0-9+.-]*):/i.exec(href)?.[1] ?? "").toLowerCase();
    if (scheme && scheme !== "http" && scheme !== "https" && scheme !== "mailto") return match;
    return `<a class="wb-md-link" href="${href}" target="_blank" rel="noopener noreferrer">${label}</a>`;
  });
  // 粗体先于斜体（** 消费后剩余单星对才按斜体处理）。
  working = working.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  working = working.replace(/__([^_\n]+)__/g, "<strong>$1</strong>");
  working = working.replace(/\*([^*\n]+)\*/g, "<em>$1</em>");
  working = working.replace(/(^|[\s(])_([^_\n]+)_(?=$|[\s).,;:!?])/g, "$1<em>$2</em>");
  // 还原行内代码占位符。
  working = working.replace(new RegExp(`${CODE_SLOT}(\\d+)${CODE_SLOT}`, "g"), (_match, index: string) => codeSpans[Number(index)] ?? "");
  return working;
}

interface InlineBlock {
  kind: "inline";
  lines: string[];
}

interface ListBlock {
  kind: "list";
  ordered: boolean;
  items: string[];
}

interface QuoteBlock {
  kind: "quote";
  lines: string[];
}

interface FenceBlock {
  kind: "fence";
  content: string;
}

type Block = InlineBlock | ListBlock | QuoteBlock | FenceBlock;

/** 行聚合：把连续同类行折叠成块，再统一产出 HTML（段落/列表/引用/围栏码）。 */
function groupBlocks(lines: string[]): Block[] {
  const blocks: Block[] = [];
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i] ?? "";
    const fence = /^ {0,3}(`{3,}|~{3,})/.exec(line);
    if (fence) {
      const marker = fence[1] ?? "";
      const close = new RegExp(`^ {0,3}[${marker[0] === "~" ? "~" : "`"}]{${marker.length},}\\s*$`);
      const buffer: string[] = [];
      i += 1;
      while (i < lines.length && !close.test(lines[i] ?? "")) {
        buffer.push(lines[i] ?? "");
        i += 1;
      }
      blocks.push({ kind: "fence", content: buffer.join("\n") });
      continue;
    }
    const quote = /^ {0,3}>(?: ?)(.*)$/.exec(line);
    if (quote) {
      const inner: string[] = [quote[1] ?? ""];
      while (i + 1 < lines.length) {
        const next = /^ {0,3}>(?: ?)(.*)$/.exec(lines[i + 1] ?? "");
        if (!next) break;
        inner.push(next[1] ?? "");
        i += 1;
      }
      blocks.push({ kind: "quote", lines: inner });
      continue;
    }
    const bullet = /^ {0,3}[-*+] +(.*)$/.exec(line);
    if (bullet) {
      const items = [bullet[1] ?? ""];
      while (i + 1 < lines.length) {
        const next = /^ {0,3}[-*+] +(.*)$/.exec(lines[i + 1] ?? "");
        if (!next) break;
        items.push(next[1] ?? "");
        i += 1;
      }
      blocks.push({ kind: "list", ordered: false, items });
      continue;
    }
    const numbered = /^ {0,3}\d+[.)] +(.*)$/.exec(line);
    if (numbered) {
      const items = [numbered[1] ?? ""];
      while (i + 1 < lines.length) {
        const next = /^ {0,3}\d+[.)] +(.*)$/.exec(lines[i + 1] ?? "");
        if (!next) break;
        items.push(next[1] ?? "");
        i += 1;
      }
      blocks.push({ kind: "list", ordered: true, items });
      continue;
    }
    // 其余非空行并入当前段落块；空行断段。
    if (line.trim() === "") {
      if (blocks.length && blocks[blocks.length - 1]?.kind === "inline" && (blocks[blocks.length - 1] as InlineBlock).lines.length) {
        blocks.push({ kind: "inline", lines: [] });
      }
      continue;
    }
    const last = blocks[blocks.length - 1];
    if (last?.kind === "inline" && last.lines.length) {
      last.lines.push(line);
    } else {
      blocks.push({ kind: "inline", lines: [line] });
    }
  }
  return blocks;
}

function renderBlocks(lines: string[]): string {
  const out: string[] = [];
  for (const block of groupBlocks(lines)) {
    if (block.kind === "fence") {
      out.push(`<pre class="wb-md-code"><code>${escapeHtml(block.content)}</code></pre>`);
      continue;
    }
    if (block.kind === "quote") {
      out.push(`<blockquote class="wb-md-quote">${renderBlocks(block.lines)}</blockquote>`);
      continue;
    }
    if (block.kind === "list") {
      const tag = block.ordered ? "ol" : "ul";
      out.push(`<${tag} class="wb-md-list">${block.items.map((item) => `<li>${renderInline(item)}</li>`).join("")}</${tag}>`);
      continue;
    }
    if (!block.lines.length) continue;
    // 单行特殊块（转义后的原文上匹配，标签/星号不受转义影响）。
    const sole = block.lines.length === 1 ? block.lines[0] ?? "" : "";
    const heading = /^(#{1,6}) (.*)$/.exec(sole);
    if (heading) {
      const level = (heading[1] ?? "#").length;
      out.push(`<h${level} class="wb-md-h">${renderInline(heading[2] ?? "")}</h${level}>`);
      continue;
    }
    if (/^ {0,3}(?:-{3,}|\*{3,}|_{3,})\s*$/.test(sole)) {
      out.push('<hr class="wb-md-hr">');
      continue;
    }
    out.push(`<p class="wb-md-p">${block.lines.map(renderInline).join("<br>")}</p>`);
  }
  return out.join("\n");
}

function renderMarkdown(text: string): string {
  // 块级结构检测基于原文（转义在叶子输出时进行，见 renderInline/围栏码分支）。
  return renderBlocks(text.replace(/\r\n?/g, "\n").replace(/\u0000/g, "").split("\n"));
}

const parsed = computed<{ html: string; failed: boolean }>(() => {
  try {
    return { html: renderMarkdown(props.text), failed: false };
  } catch {
    return { html: "", failed: true };
  }
});

watch(
  () => parsed.value.failed,
  (failed) => {
    if (failed) emit("render-error");
  },
  { immediate: true },
);
</script>

<template>
  <div class="wb-md-render" data-test="md-render">
    <!-- 解析异常兜底：插值输出原文（Vue 自动转义），PreviewPane 同时回退源码视图。 -->
    <pre v-if="parsed.failed" class="wb-md-render-failed">{{ text }}</pre>
    <div v-else class="wb-md-body" v-html="parsed.html"></div>
  </div>
</template>
