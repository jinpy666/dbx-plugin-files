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

const TEXT_MIMES: Record<string, string> = {
  bat: "application/x-bat",
  c: "text/x-c",
  cc: "text/x-c++",
  cmake: "text/x-cmake",
  cmd: "application/x-bat",
  conf: "text/plain",
  cpp: "text/x-c++",
  cs: "text/x-csharp",
  css: "text/css",
  dart: "application/dart",
  diff: "text/x-diff",
  go: "text/x-go",
  groovy: "text/x-groovy",
  h: "text/x-c",
  hpp: "text/x-c++",
  hs: "text/x-haskell",
  htm: "text/html",
  html: "text/html",
  ini: "text/plain",
  java: "text/x-java-source",
  jl: "text/x-julia",
  js: "text/javascript",
  json: "application/json",
  jsx: "text/jsx",
  kt: "text/x-kotlin",
  kts: "text/x-kotlin",
  less: "text/less",
  log: "text/plain",
  lua: "text/x-lua",
  m: "text/x-objectivec",
  markdown: "text/markdown",
  md: "text/markdown",
  mjs: "text/javascript",
  mm: "text/x-objectivec",
  patch: "text/x-diff",
  php: "application/x-httpd-php",
  pl: "text/x-perl",
  pm: "text/x-perl",
  proto: "text/x-protobuf",
  ps1: "application/x-powershell",
  py: "text/x-python",
  r: "text/x-r",
  rb: "text/x-ruby",
  rs: "text/x-rust",
  scala: "text/x-scala",
  scss: "text/x-scss",
  sh: "application/x-sh",
  sql: "application/sql",
  swift: "text/x-swift",
  text: "text/plain",
  toml: "application/toml",
  ts: "text/typescript",
  tsx: "text/tsx",
  txt: "text/plain",
  vue: "text/x-vue",
  xml: "application/xml",
  yaml: "text/yaml",
  yml: "text/yaml",
  zsh: "application/x-sh",
};

/** Structured data formats whose viewer rendering beats plain text. HTML/HTM
 * stay on CodeMirror: the viewer's HTML preview hardcodes a white iframe
 * background that cannot follow the host dark theme. */
const VIEWER_TEXT_MIMES: Record<string, string> = {
  csv: "text/csv",
  tsv: "text/tab-separated-values",
};

const OFFICE_MIMES: Record<string, string> = {
  doc: "application/msword",
  docm: "application/vnd.ms-word.document.macroEnabled.12",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  odg: "application/vnd.oasis.opendocument.graphics",
  odp: "application/vnd.oasis.opendocument.presentation",
  ods: "application/vnd.oasis.opendocument.spreadsheet",
  odt: "application/vnd.oasis.opendocument.text",
  pot: "application/vnd.ms-powerpoint",
  potx: "application/vnd.openxmlformats-officedocument.presentationml.template",
  pps: "application/vnd.ms-powerpoint",
  ppsx: "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
  ppt: "application/vnd.ms-powerpoint",
  pptm: "application/vnd.ms-powerpoint.presentation.macroEnabled.12",
  pptx: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  xls: "application/vnd.ms-excel",
  xlsm: "application/vnd.ms-excel.sheet.macroEnabled.12",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
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
  mkv: "video/x-matroska",
  mov: "video/quicktime",
  mp3: "audio/mpeg",
  mp4: "video/mp4",
  oga: "audio/ogg",
  ogg: "audio/ogg",
  opus: "audio/opus",
  wav: "audio/wav",
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
