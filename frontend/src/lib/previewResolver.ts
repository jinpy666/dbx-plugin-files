import { archiveKind } from "./archive";

export type PreviewKind = "image" | "text" | "archive" | "office" | "pdf" | "media" | "unknown";
export type PreviewStrategy = "file-viewer";
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
  /** Read-only previews are deliberately delegated to the host file viewer. */
  previewStrategy: PreviewStrategy;
  /** The editor used when an editable text file enters edit mode. */
  editorStrategy: EditorStrategy;
  /** Unknown formats may still be attempted by file-viewer. */
  fallback: boolean;
  fallbackReason: PreviewFallbackReason;
}

type KnownPreview = {
  kind: Exclude<PreviewKind, "unknown" | "archive">;
  mime: string;
  editable?: boolean;
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
  c: "text/x-c",
  cc: "text/x-c++",
  cpp: "text/x-c++",
  css: "text/css",
  csv: "text/csv",
  h: "text/x-c",
  hpp: "text/x-c++",
  htm: "text/html",
  html: "text/html",
  ini: "text/plain",
  java: "text/x-java-source",
  js: "text/javascript",
  json: "application/json",
  jsx: "text/jsx",
  less: "text/less",
  log: "text/plain",
  markdown: "text/markdown",
  md: "text/markdown",
  mjs: "text/javascript",
  py: "text/x-python",
  r: "text/x-r",
  rb: "text/x-ruby",
  rs: "text/x-rust",
  scss: "text/x-scss",
  sh: "application/x-sh",
  sql: "application/sql",
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
  ...Object.fromEntries(Object.entries(IMAGE_MIMES).map(([extension, mime]) => [extension, { kind: "image", mime }])),
  ...Object.fromEntries(Object.entries(TEXT_MIMES).map(([extension, mime]) => [extension, { kind: "text", mime, editable: true }])),
  ...Object.fromEntries(Object.entries(OFFICE_MIMES).map(([extension, mime]) => [extension, { kind: "office", mime }])),
  ...Object.fromEntries(Object.entries(PDF_MIMES).map(([extension, mime]) => [extension, { kind: "pdf", mime }])),
  ...Object.fromEntries(Object.entries(MEDIA_MIMES).map(([extension, mime]) => [extension, { kind: "media", mime }])),
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
    previewStrategy: "file-viewer",
    editorStrategy: null,
    fallback: true,
    fallbackReason: "unknown-extension",
  };
}

/**
 * Resolve a path into rendering and editing capabilities without importing a renderer.
 *
 * Every result is safe for a read-only file viewer. Only recognized text/code files
 * expose CodeMirror as an editing strategy; an unknown extension remains a viewer
 * fallback instead of being treated as editable text.
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
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
      fallbackReason: null,
    };
  }

  const known = KNOWN_PREVIEWS[extension];
  if (!known) return unknownPreview(extension);

  const editable = known.editable === true;
  return {
    extension,
    mime: known.mime,
    kind: known.kind,
    editable,
    previewStrategy: "file-viewer",
    editorStrategy: editable ? "codemirror" : null,
    fallback: false,
    fallbackReason: null,
  };
}
