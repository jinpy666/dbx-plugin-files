// CodeMirror 语言包白名单：替代 @codemirror/language-data 全量语言索引。
// language-data 收录 130+ 语言（含 110 个 @codemirror/legacy-modes 长尾模式），
// 其 load() 内的动态 import 在 inlineDynamicImports 构建下会被全量内联
// （约 2.5-4MB）；此处按 Files Studio 真实场景显式保留常用语言子集。
// 未命中文件名时 LanguageDescription.matchFilename 返回 undefined，
// 编辑器退化为无高亮纯文本（basicSetup 行号/折叠等基础能力不受影响）。
// Shell/TOML 复用 legacy-modes 的单文件深路径 import，只引入这两个模式。
import { LanguageDescription, LanguageSupport, StreamLanguage } from "@codemirror/language";

export const editorLanguages: LanguageDescription[] = [
  LanguageDescription.of({
    name: "JSON",
    alias: ["json5", "jsonc"],
    extensions: ["json", "map"],
    load: () => import("@codemirror/lang-json").then((m) => m.json()),
  }),
  LanguageDescription.of({
    name: "JavaScript",
    alias: ["ecmascript", "js", "node"],
    extensions: ["js", "mjs", "cjs", "jsx"],
    load: () => import("@codemirror/lang-javascript").then((m) => m.javascript()),
  }),
  LanguageDescription.of({
    name: "TypeScript",
    alias: ["ts"],
    extensions: ["ts", "mts", "cts", "tsx"],
    load: () => import("@codemirror/lang-javascript").then((m) => m.javascript({ typescript: true })),
  }),
  LanguageDescription.of({
    name: "HTML",
    alias: ["xhtml"],
    extensions: ["html", "htm", "handlebars", "hbs"],
    load: () => import("@codemirror/lang-html").then((m) => m.html()),
  }),
  LanguageDescription.of({
    name: "CSS",
    extensions: ["css"],
    load: () => import("@codemirror/lang-css").then((m) => m.css()),
  }),
  LanguageDescription.of({
    name: "Python",
    extensions: ["py", "pyw"],
    load: () => import("@codemirror/lang-python").then((m) => m.python()),
  }),
  LanguageDescription.of({
    name: "Rust",
    extensions: ["rs"],
    load: () => import("@codemirror/lang-rust").then((m) => m.rust()),
  }),
  LanguageDescription.of({
    name: "SQL",
    extensions: ["sql"],
    load: () => import("@codemirror/lang-sql").then((m) => m.sql()),
  }),
  LanguageDescription.of({
    name: "Markdown",
    extensions: ["md", "markdown", "mkd"],
    load: () => import("@codemirror/lang-markdown").then((m) => m.markdown()),
  }),
  LanguageDescription.of({
    name: "XML",
    alias: ["rss", "wsdl", "xsd"],
    extensions: ["xml", "xsl", "xsd", "svg"],
    load: () => import("@codemirror/lang-xml").then((m) => m.xml()),
  }),
  LanguageDescription.of({
    name: "YAML",
    alias: ["yml"],
    extensions: ["yaml", "yml"],
    load: () => import("@codemirror/lang-yaml").then((m) => m.yaml()),
  }),
  LanguageDescription.of({
    name: "C",
    extensions: ["c", "h", "ino"],
    load: () => import("@codemirror/lang-cpp").then((m) => m.cpp()),
  }),
  LanguageDescription.of({
    name: "C++",
    alias: ["cpp"],
    extensions: ["cpp", "c++", "cc", "cxx", "hpp", "h++", "hh", "hxx"],
    load: () => import("@codemirror/lang-cpp").then((m) => m.cpp()),
  }),
  LanguageDescription.of({
    name: "Java",
    extensions: ["java"],
    load: () => import("@codemirror/lang-java").then((m) => m.java()),
  }),
  LanguageDescription.of({
    name: "Go",
    extensions: ["go"],
    load: () => import("@codemirror/lang-go").then((m) => m.go()),
  }),
  LanguageDescription.of({
    name: "PHP",
    extensions: ["php", "php3", "php4", "php5", "php7", "phtml"],
    load: () => import("@codemirror/lang-php").then((m) => m.php()),
  }),
  LanguageDescription.of({
    name: "Shell",
    alias: ["bash", "sh", "zsh"],
    extensions: ["sh", "ksh", "bash"],
    filename: /^PKGBUILD$/,
    load: () =>
      import("@codemirror/legacy-modes/mode/shell").then(
        (m) => new LanguageSupport(StreamLanguage.define(m.shell)),
      ),
  }),
  LanguageDescription.of({
    name: "TOML",
    extensions: ["toml"],
    load: () =>
      import("@codemirror/legacy-modes/mode/toml").then(
        (m) => new LanguageSupport(StreamLanguage.define(m.toml)),
      ),
  }),
];
