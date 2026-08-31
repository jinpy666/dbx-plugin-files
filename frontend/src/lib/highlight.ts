// 预览代码高亮：highlight.js core + 常用语言子集（按扩展名映射，体积可控）。
// 未识别的扩展返回 null（纯文本渲染）；不做 auto-detect，避免误判与开销。
// 输出经 hljs 转义，v-html 安全。
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import css from "highlight.js/lib/languages/css";
import go from "highlight.js/lib/languages/go";
import ini from "highlight.js/lib/languages/ini";
import java from "highlight.js/lib/languages/java";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import markdown from "highlight.js/lib/languages/markdown";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";

hljs.registerLanguage("bash", bash);
hljs.registerLanguage("css", css);
hljs.registerLanguage("go", go);
hljs.registerLanguage("ini", ini);
hljs.registerLanguage("java", java);
hljs.registerLanguage("javascript", javascript);
hljs.registerLanguage("json", json);
hljs.registerLanguage("markdown", markdown);
hljs.registerLanguage("python", python);
hljs.registerLanguage("rust", rust);
hljs.registerLanguage("sql", sql);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("yaml", yaml);

/** 扩展名 → 已注册语言；未列出的一律纯文本。 */
const EXT_LANGUAGE: Record<string, string> = {
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  jsx: "javascript",
  ts: "typescript",
  mts: "typescript",
  cts: "typescript",
  tsx: "typescript",
  json: "json",
  py: "python",
  rs: "rust",
  go: "go",
  java: "java",
  sh: "bash",
  bash: "bash",
  zsh: "bash",
  sql: "sql",
  xml: "xml",
  html: "xml",
  htm: "xml",
  svg: "xml",
  css: "css",
  yml: "yaml",
  yaml: "yaml",
  md: "markdown",
  markdown: "markdown",
  toml: "ini",
  ini: "ini",
  conf: "ini",
  cnf: "ini",
};

export function extensionOf(fileName: string): string {
  const name = fileName.split("/").pop() ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

/** 返回高亮 HTML；语言未识别或高亮失败返回 null（调用方回退纯文本）。 */
export function highlightCode(code: string, fileName: string): string | null {
  const language = EXT_LANGUAGE[extensionOf(fileName)];
  if (!language || !hljs.getLanguage(language)) return null;
  try {
    return hljs.highlight(code, { language, ignoreIllegals: true }).value;
  } catch {
    return null;
  }
}
