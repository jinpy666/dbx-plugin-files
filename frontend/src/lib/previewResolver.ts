import { archiveKind } from "./archive";

export type PreviewKind = "image" | "text" | "archive" | "office" | "pdf" | "media" | "unknown";
/**
 * Two-tier rendering split: text/code stay on CodeMirror, images on the native
 * element, archives on files/archiveList, binary fallback on hex — only formats
 * CodeMirror cannot render (Office/PDF/media/sandboxed HTML/CSV) go to file-viewer.
 */
export type PreviewStrategy = "codemirror" | "image" | "archive" | "hex" | "file-viewer";
export type EditorStrategy = "codemirror" | null;
export type PreviewFallbackReason = "unknown-extension" | null;

export interface PreviewResolution {
  /** The normalized final extension, without a leading dot. Compound archives keep both suffixes. */
  extension: string;
  /** A best-effort MIME type from the extension, or null when the extension is unknown. */
  mime: string | null;
  kind: PreviewKind;
  /** Only known text/code extensions are editable. */
  editable: boolean;
  /** Rendering tier chosen for this file; see PreviewStrategy. */
  previewStrategy: PreviewStrategy;
  /** The editor used when an editable text file enters edit mode. */
  editorStrategy: EditorStrategy;
  /** Unknown extensions keep the hex/text heuristic instead of the viewer. */
  fallback: boolean;
  fallbackReason: PreviewFallbackReason;
}

type KnownPreview = {
  kind: Exclude<PreviewKind, "unknown" | "archive">;
  mime: string;
  editable?: boolean;
  strategy?: Exclude<PreviewStrategy, "codemirror" | "archive" | "hex">;
};

const IMAGE_MIMES: Record<string, string> = {
  avif: "image/avif",
  bmp: "image/bmp",
  gif: "image/gif",
  ico: "image/x-icon",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
  png: "image/png",
  svg: "image/svg+xml",
  webp: "image/webp",
};

/** 浏览器 <img> 解不了的图片格式，交给 renderer-image 自带的解码器。 */
const VIEWER_IMAGE_MIMES: Record<string, string> = {
  heic: "image/heic",
  heif: "image/heif",
  jxl: "image/jxl",
  tif: "image/tiff",
  tiff: "image/tiff",
};

const TEXT_MIMES: Record<string, string> = {
  adoc: "text/x-asciidoc",
  astro: "text/plain",
  bat: "application/x-bat",
  bash: "application/x-sh",
  bicep: "text/plain",
  bib: "text/plain",
  c: "text/x-c",
  cc: "text/x-c++",
  cfg: "text/plain",
  cjs: "text/javascript",
  cmake: "text/x-cmake",
  cmd: "application/x-bat",
  conf: "text/plain",
  cpp: "text/x-c++",
  cs: "text/x-csharp",
  css: "text/css",
  dart: "application/dart",
  diff: "text/x-diff",
  drawio: "application/xml",
  eex: "text/plain",
  ejs: "text/plain",
  el: "text/x-elisp",
  elm: "text/plain",
  erb: "text/plain",
  ex: "text/x-elixir",
  exs: "text/x-elixir",
  f: "text/x-fortran",
  f03: "text/x-fortran",
  f08: "text/x-fortran",
  f90: "text/x-fortran",
  f95: "text/x-fortran",
  fish: "application/x-sh",
  fs: "text/x-fsharp",
  fsi: "text/x-fsharp",
  fsx: "text/x-fsharp",
  gemspec: "text/x-ruby",
  glsl: "text/plain",
  go: "text/x-go",
  gradle: "text/x-groovy",
  graphql: "application/graphql",
  groovy: "text/x-groovy",
  gv: "text/x-graphviz",
  h: "text/x-c",
  hbs: "text/x-handlebars-template",
  hlsl: "text/plain",
  hpp: "text/x-c++",
  hs: "text/x-haskell",
  htm: "text/html",
  html: "text/html",
  http: "text/plain",
  hx: "text/x-haxe",
  inc: "text/plain",
  ini: "text/plain",
  java: "text/x-java-source",
  j2: "text/plain",
  jade: "text/x-pug",
  jinja: "text/plain",
  jl: "text/x-julia",
  js: "text/javascript",
  json: "application/json",
  json5: "application/json5",
  jsonc: "application/json",
  jsx: "text/jsx",
  jsp: "text/x-jsp",
  kt: "text/x-kotlin",
  kts: "text/x-kotlin",
  less: "text/less",
  lisp: "text/x-lisp",
  lrc: "text/plain",
  lock: "text/plain",
  log: "text/plain",
  lua: "text/x-lua",
  m: "text/x-objectivec",
  m3u: "text/plain",
  m3u8: "text/plain",
  markdown: "text/markdown",
  md: "text/markdown",
  mdown: "text/markdown",
  mdx: "text/markdown",
  mjs: "text/javascript",
  mk: "text/x-makefile",
  mkd: "text/markdown",
  ml: "text/x-ocaml",
  mli: "text/x-ocaml",
  mm: "text/x-objectivec",
  mts: "text/typescript",
  nim: "text/plain",
  nix: "text/plain",
  pas: "text/x-pascal",
  patch: "text/x-diff",
  php: "application/x-httpd-php",
  pl: "text/x-perl",
  plist: "application/xml",
  pm: "text/x-perl",
  pp: "text/x-puppet",
  prisma: "text/plain",
  properties: "text/x-properties",
  proto: "text/x-protobuf",
  ps1: "application/x-powershell",
  psd1: "application/x-powershell",
  psm1: "application/x-powershell",
  py: "text/x-python",
  pyi: "text/x-python",
  r: "text/x-r",
  rake: "text/x-ruby",
  rb: "text/x-ruby",
  rs: "text/x-rust",
  rst: "text/x-rst",
  sass: "text/x-sass",
  sbt: "text/x-scala",
  scala: "text/x-scala",
  sc: "text/x-scala",
  scss: "text/x-scss",
  service: "text/plain",
  sh: "application/x-sh",
  sml: "text/x-sml",
  sol: "text/plain",
  sql: "application/sql",
  srt: "text/plain",
  styl: "text/x-styl",
  svelte: "text/plain",
  swift: "text/x-swift",
  t: "text/x-perl",
  tf: "text/x-hcl",
  tfvars: "text/x-hcl",
  thrift: "text/plain",
  tmpl: "text/plain",
  toml: "application/toml",
  ts: "text/typescript",
  tsx: "text/tsx",
  twig: "text/x-twig",
  txt: "text/plain",
  v: "text/x-verilog",
  vb: "text/x-vb",
  vbs: "text/x-vbscript",
  vhd: "text/x-vhdl",
  vhdl: "text/x-vhdl",
  vim: "text/x-vim",
  vue: "text/x-vue",
  vtt: "text/vtt",
  wat: "text/plain",
  wxml: "application/xml",
  wxss: "text/css",
  xhtml: "application/xhtml+xml",
  xml: "application/xml",
  xsd: "application/xml",
  xsl: "application/xml",
  xslt: "application/xml",
  yaml: "text/yaml",
  yml: "text/yaml",
  zig: "text/plain",
  zsh: "application/x-sh",
};

/** Structured data formats whose viewer rendering beats plain text. HTML/HTM
 * stay on CodeMirror: the viewer's HTML preview hardcodes a white iframe
 * background that cannot follow the host dark theme. */
const VIEWER_TEXT_MIMES: Record<string, string> = {
  csv: "text/csv",
  ipynb: "application/x-ipynb+json",
  tsv: "text/tab-separated-values",
};

const OFFICE_MIMES: Record<string, string> = {
  dbf: "application/x-dbf",
  doc: "application/msword",
  docm: "application/vnd.ms-word.document.macroEnabled.12",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  dot: "application/msword",
  dotm: "application/vnd.ms-word.template.macroEnabled.12",
  dotx: "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
  fods: "application/vnd.oasis.opendocument.spreadsheet",
  ofd: "application/ofd",
  odg: "application/vnd.oasis.opendocument.graphics",
  odp: "application/vnd.oasis.opendocument.presentation",
  ods: "application/vnd.oasis.opendocument.spreadsheet",
  odt: "application/vnd.oasis.opendocument.text",
  pot: "application/vnd.ms-powerpoint",
  potm: "application/vnd.ms-powerpoint.template.macroEnabled.12",
  potx: "application/vnd.openxmlformats-officedocument.presentationml.template",
  pps: "application/vnd.ms-powerpoint",
  ppsx: "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
  ppsm: "application/vnd.ms-powerpoint.slideshow.macroEnabled.12",
  ppt: "application/vnd.ms-powerpoint",
  pptm: "application/vnd.ms-powerpoint.presentation.macroEnabled.12",
  pptx: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  xla: "application/vnd.ms-excel",
  xlam: "application/vnd.ms-excel.addin.macroEnabled.12",
  xls: "application/vnd.ms-excel",
  xlsb: "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
  xlsm: "application/vnd.ms-excel.sheet.macroEnabled.12",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  xlt: "application/vnd.ms-excel",
  xltm: "application/vnd.ms-excel.template.macroEnabled.12",
  xltx: "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
  xmind: "application/vnd.xmind",
};

const PDF_MIMES: Record<string, string> = {
  pdf: "application/pdf",
};

const MEDIA_MIMES: Record<string, string> = {
  aac: "audio/aac",
  avi: "video/x-msvideo",
  flac: "audio/flac",
  m4a: "audio/mp4",
  m4v: "video/x-m4v",
  mid: "audio/midi",
  midi: "audio/midi",
  mkv: "video/x-matroska",
  mov: "video/quicktime",
  mp3: "audio/mpeg",
  mp4: "video/mp4",
  oga: "audio/ogg",
  ogg: "audio/ogg",
  opus: "audio/opus",
  wav: "audio/wav",
  weba: "audio/webm",
  webm: "video/webm",
  wma: "audio/x-ms-wma",
  wmv: "video/x-ms-wmv",
};

const ARCHIVE_MIMES: Record<string, string> = {
  tar: "application/x-tar",
  "tar.gz": "application/gzip",
  tgz: "application/gzip",
  zip: "application/zip",
};

const KNOWN_PREVIEWS: Record<string, KnownPreview> = {
  ...Object.fromEntries(Object.entries(IMAGE_MIMES).map(([extension, mime]) => [extension, { kind: "image", mime, strategy: "image" as const }])),
  ...Object.fromEntries(Object.entries(VIEWER_IMAGE_MIMES).map(([extension, mime]) => [extension, { kind: "image", mime, strategy: "file-viewer" as const }])),
  ...Object.fromEntries(Object.entries(TEXT_MIMES).map(([extension, mime]) => [extension, { kind: "text", mime, editable: true }])),
  ...Object.fromEntries(Object.entries(VIEWER_TEXT_MIMES).map(([extension, mime]) => [extension, { kind: "text", mime, strategy: "file-viewer" as const }])),
  ...Object.fromEntries(Object.entries(OFFICE_MIMES).map(([extension, mime]) => [extension, { kind: "office", mime, strategy: "file-viewer" as const }])),
  ...Object.fromEntries(Object.entries(PDF_MIMES).map(([extension, mime]) => [extension, { kind: "pdf", mime, strategy: "file-viewer" as const }])),
  ...Object.fromEntries(Object.entries(MEDIA_MIMES).map(([extension, mime]) => [extension, { kind: "media", mime, strategy: "file-viewer" as const }])),
};

function extensionFor(path: string): string {
  const basename = path.split(/[\\/]/).pop() ?? "";
  const lower = basename.toLowerCase();
  for (const compound of ["tar.gz"]) {
    if (lower.endsWith(`.${compound}`)) return compound;
  }
  const dot = lower.lastIndexOf(".");
  // A leading dot denotes a dotfile, not an extension (for example .env).
  if (dot <= 0 || dot === lower.length - 1) return "";
  return lower.slice(dot + 1);
}

function unknownPreview(extension: string): PreviewResolution {
  return {
    extension,
    mime: null,
    kind: "unknown",
    editable: false,
    // Unknown extensions keep the printable-ratio heuristic: readable text is
    // shown as read-only text, everything else falls back to a hex dump.
    previewStrategy: "hex",
    editorStrategy: null,
    fallback: true,
    fallbackReason: "unknown-extension",
  };
}

/**
 * Resolve a path into rendering and editing capabilities without importing a renderer.
 *
 * Text/code preview and editing stay on CodeMirror; images use the native
 * element; archives use files/archiveList; unknown extensions use the text/hex
 * heuristic. Only formats CodeMirror cannot render (Office/PDF/media/sandboxed
 * HTML/CSV) are delegated to the embedded file viewer.
 */
export function resolvePreview(path: string): PreviewResolution {
  const extension = extensionFor(path);
  const archive = archiveKind(path);
  if (archive) {
    return {
      extension,
      mime: ARCHIVE_MIMES[extension] ?? null,
      kind: "archive",
      editable: false,
      previewStrategy: "archive",
      editorStrategy: null,
      fallback: false,
      fallbackReason: null,
    };
  }

  const known = KNOWN_PREVIEWS[extension];
  if (!known) return unknownPreview(extension);

  const editable = known.editable === true;
  const strategy: PreviewStrategy = known.strategy ?? (editable ? "codemirror" : "hex");
  return {
    extension,
    mime: known.mime,
    kind: known.kind,
    editable,
    previewStrategy: strategy,
    editorStrategy: editable ? "codemirror" : null,
    fallback: false,
    fallbackReason: null,
  };
}
