// rclone 后端类型建议与自定义透传编辑器的 JSON 示例（纯 UI 糖）。
// 后端零翻译：rclone-custom 透传把 service 直接当作 rclone backend type、
// config JSON 原样作为 `config/create` 参数集；这里只提供 datalist 建议
// 与常用后端的参数示例，键名一律以 `rclone config providers` 的实测输出
// 为准（v1.75.1），不得凭记忆新增（见 docs/IMPL_PLAN_RCLONE §4）。

/**
 * rclone 后端类型全集（`rclone config providers` on v1.75.1 的别名与规范
 * 名，仅收小写字母/数字——与 sidecar `custom_rclone_type` 的校验规则一致，
 * 因此带空格的规范类型如 "google photos" 不可经 rclone-custom 透传，只能走
 * 一级协议）。透传可达任意类型——这份清单只是 datalist 建议，不是白名单：
 * 后端在使用时按实际 rclone 校验，新版本新增的后端可直接手输（见
 * https://rclone.org/overview/）。
 */
export const RCLONE_BACKEND_TYPES: ReadonlySet<string> = new Set([
  "alias", "archive", "azureblob", "azurefiles", "b2", "box", "cache", "chunker",
  "cloudinary", "combine", "compress", "crypt", "doi", "drime", "drive", "dropbox",
  "fichier", "filefabric", "filelu", "filen", "filescom", "ftp", "gcs", "gofile",
  "gphotos", "hasher", "hdfs", "hidrive", "http", "huaweidrive", "iclouddrive",
  "imagekit", "internetarchive", "internxt", "jottacloud", "koofr", "linkbox",
  "local", "mailru", "mega", "memory", "netstorage", "oos", "onedrive", "opendrive",
  "oracleobjectstorage", "pcloud", "pikpak", "pixeldrain", "premiumizeme",
  "protondrive", "putio", "qingstor", "quatrix", "s3", "seafile", "sftp", "shade",
  "sharefile", "sia", "smb", "storj", "sugarsync", "swift", "tardigrade", "ulozto",
  "union", "webdav", "yandex", "zoho",
]);

/** service 输入框的 datalist 建议：rclone 后端全集。 */
export function customServiceSuggestions(): string[] {
  return [...RCLONE_BACKEND_TYPES].sort();
}

/**
 * 已升为一级协议的通用 rclone 后端：协议值即 rclone backend type，参数走
 * `config` JSON 字段。与后端 `model::GENERIC_PROTOCOLS` 逐项对齐（53 项，
 * 含带空格的规范类型 "google photos"）；别名形式（gphotos/oos）与未升级的
 * cache 不在此列。
 */
export const GENERIC_PROTOCOL_IDS: ReadonlySet<string> = new Set([
  "alias", "archive", "azurefiles", "b2", "box", "chunker", "cloudinary",
  "combine", "compress", "crypt", "doi", "drime", "fichier", "filefabric",
  "filelu", "filen", "filescom", "gofile", "google photos", "hasher", "hdfs",
  "hidrive", "http", "huaweidrive", "iclouddrive", "imagekit",
  "internetarchive", "internxt", "jottacloud", "linkbox", "mailru", "mega",
  "memory", "netstorage", "opendrive", "oracleobjectstorage", "pikpak",
  "pixeldrain", "premiumizeme", "protondrive", "putio", "qingstor", "quatrix",
  "shade", "sharefile", "sia", "storj", "sugarsync", "swift", "tardigrade",
  "ulozto", "union", "zoho",
]);

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
  oracleobjectstorage:
    '{\n  "provider": "user_principal_auth",\n  "namespace": "...",\n  "region": "us-ashburn-1",\n  "compartment": "..."\n}',
};
