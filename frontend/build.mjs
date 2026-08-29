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
const inlineScript = script.replace(/<\/script/gi, "<\\/script");
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
if ((html.match(/<script\b/gi) ?? []).length !== 1 || (html.match(/<\/script>/gi) ?? []).length !== 1) {
  throw new Error("Self-contained UI contains an invalid script structure");
}

await fs.mkdir(output, { recursive: true });
await fs.writeFile(path.join(output, "index.html"), html, "utf8");
console.log(`Wrote self-contained plugin UI to ${path.join(output, "index.html")}`);
