// rclone 后端类型建议与自定义透传编辑器的 JSON 示例（纯 UI 糖）。
// 后端零翻译：rclone-custom 透传把 service 直接当作 rclone backend type、
// config JSON 原样作为 `config/create` 参数集；这里只提供 datalist 建议
// 与常用后端的参数示例，键名一律以 `rclone config providers` 的实测输出
// 为准（v1.75.1），不得凭记忆新增（见 docs/IMPL_PLAN_RCLONE §4）。

/**
 * rclone 后端类型全集（`rclone config providers` on v1.75.1；gcs/gphotos/oos
 * 为 provider 展示名对应的真实 backend type）。透传可达任意类型——这份清单
 * 只是 datalist 建议，不是白名单：后端在使用时按实际 rclone 校验，新版本
 * 新增的后端可直接手输（见 https://rclone.org/overview/）。
 */
export const RCLONE_BACKEND_TYPES: ReadonlySet<string> = new Set([
  "alias", "archive", "azureblob", "azurefiles", "b2", "box", "cache", "chunker",
  "cloudinary", "combine", "compress", "crypt", "doi", "drime", "drive", "dropbox",
  "fichier", "filefabric", "filelu", "filen", "filescom", "ftp", "gcs", "gofile",
  "gphotos", "hasher", "hdfs", "hidrive", "http", "huaweidrive", "iclouddrive",
  "imagekit", "internetarchive", "internxt", "jottacloud", "koofr", "linkbox",
  "local", "mailru", "mega", "memory", "netstorage", "onedrive", "oos", "opendrive",
  "pcloud", "pikpak", "pixeldrain", "premiumizeme", "protondrive", "putio",
  "qingstor", "quatrix", "s3", "seafile", "sftp", "shade", "sharefile", "sia",
  "smb", "storj", "sugarsync", "swift", "tardigrade", "ulozto", "union", "webdav",
  "yandex", "zoho",
]);

/** service 输入框的 datalist 建议：rclone 后端全集。 */
export function customServiceSuggestions(): string[] {
  return [...RCLONE_BACKEND_TYPES].sort();
}

/**
 * 已升为一级协议的通用 rclone 后端（无快捷表单的后端全集）：协议值即
 * rclone backend type，参数走 `config` JSON 字段。与后端
 * `model::GENERIC_PROTOCOLS` 对齐（快捷协议已映射的类型与 memory/cache
 * 不进表单）。
 */
const QUICK_MAPPED_BACKEND_TYPES: ReadonlySet<string> = new Set([
  "local", "s3", "gcs", "azureblob", "webdav", "ftp", "sftp", "smb", "drive",
  "dropbox", "onedrive", "yandex", "seafile", "koofr", "pcloud", "memory", "cache",
]);

export const GENERIC_PROTOCOL_IDS: ReadonlySet<string> = new Set(
  [...RCLONE_BACKEND_TYPES].filter((type) => !QUICK_MAPPED_BACKEND_TYPES.has(type)),
);

/**
 * 常用后端的参数示例（JSON 编辑器的占位草稿）。键名经
 * `rclone config providers <type>` 实测核对（v1.75.1）；未列出的后端
 * 直接手写 JSON，键名见 rclone 文档。
 */
export const CUSTOM_CONFIG_HINTS: Readonly<Record<string, string>> = {
  memory: "{}",
  b2: '{\n  "account": "...",\n  "key": "..."\n}',
  mega: '{\n  "user": "...",\n  "pass": "..."\n}',
  http: '{ "url": "https://files.example.com" }',
  hdfs: '{ "namenode": "namenode.example.com:9000" }',
  azurefiles: '{\n  "account": "...",\n  "key": "..."\n}',
  protondrive: '{\n  "username": "...",\n  "password": "..."\n}',
  swift: '{\n  "user": "...",\n  "key": "...",\n  "region": "..."\n}',
};
