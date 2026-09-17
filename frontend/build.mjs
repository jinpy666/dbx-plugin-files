import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "vite";
import vue from "@vitejs/plugin-vue";

const root = path.dirname(fileURLToPath(import.meta.url));
const temporary = path.join(root, "dist");
const output = path.resolve(root, "../ui");

await build({
  root,
  configFile: false,
  plugins: [vue()],
  base: "./",
  build: {
    outDir: temporary,
    emptyOutDir: true,
    cssCodeSplit: false,
    assetsInlineLimit: 10 * 1024 * 1024,
    rollupOptions: { output: { inlineDynamicImports: true } },
  },
});

let html = await fs.readFile(path.join(temporary, "index.html"), "utf8");
const scriptMatch = html.match(/<script[^>]+src="\.\/([^"]+\.js)"[^>]*><\/script>/);
if (!scriptMatch) throw new Error("Vite output did not contain a JavaScript entry");
const script = await fs.readFile(path.join(temporary, scriptMatch[1]), "utf8");
// 内联双层转义：① `</script` 防止提前闭合；② `<!--` 防止 HTML 解析器进入
// script 注释态（highlight.js 语言定义同时含 `<!--` 与 `<script` 字面量，
// 组合起来会让真实闭合标签失效）——`\!` 在 JS 字符串/正则中均等价 `!`，语义不变。
const inlineScript = script.replace(/<\/script/gi, "<\\/script").replace(/<!--/g, "<\\!--");
html = html.replace(scriptMatch[0], () => `<script type="module">${inlineScript}</script>`);

const styleMatch = html.match(/<link[^>]+href="\.\/([^"]+\.css)"[^>]*>/);
if (styleMatch) {
  const style = await fs.readFile(path.join(temporary, styleMatch[1]), "utf8");
  const inlineStyle = style.replace(/<\/style/gi, "<\\/style");
  html = html.replace(styleMatch[0], () => `<style>${inlineStyle}</style>`);
}

if (html.includes(scriptMatch[0])) {
  throw new Error("Self-contained UI still contains the external JavaScript entry tag");
}
if (styleMatch && html.includes(styleMatch[0])) {
  throw new Error("Self-contained UI still contains the external stylesheet tag");
}
// 只统计真实标签开口（`<script` 后跟空白或 `>`）；依赖代码里的正则字面量
// （如 `<script(?=\s|>)`）后随 `(`，不应计入。
// The inlined bundle intentionally contains HTML templates as JavaScript strings
// (for example Mermaid/Railroad diagrams). Count only actual outer HTML tags by
// checking the document shell before the inlined script body; string contents are
// not part of the host document's tag structure.
const shell = html.slice(0, html.indexOf("<script type=\"module\">"));
const scriptOpenings = (shell.match(/<script(?=[\s>])/gi) ?? []).length + 1;
const scriptClosings = (html.slice(html.lastIndexOf("</script>"))).match(/<\/script>/gi)?.length ?? 0;
if (scriptOpenings !== 1 || scriptClosings !== 1) {
  throw new Error("Self-contained UI contains an invalid script structure");
}

await fs.mkdir(output, { recursive: true });
await fs.writeFile(path.join(output, "index.html"), html, "utf8");
console.log(`Wrote self-contained plugin UI to ${path.join(output, "index.html")}`);
