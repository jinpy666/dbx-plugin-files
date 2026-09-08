// OpenDAL 服务字段模板（纯 UI 糖，自 tiny-rdm rcloneConfig.js 改写）。
// 后端零翻译：字段 key 即 OpenDAL Builder 配置键，直入 Builder map。
// 快捷协议（fs/s3/webdav/ftp/sftp/smb）与 opendal-custom 透传的元数据都在这里，
// UI 列表须与 sidecar 编译期 feature 白名单保持同步（见 docs/IMPL_PLAN §2）。

export type ServiceFieldType = "text" | "password" | "boolean" | "select";

export interface ServiceField {
  key: string;
  type: ServiceFieldType;
  required?: boolean;
  placeholder?: string;
  options?: string[];
  /** secret 类字段只入宿主 binding，不进日志。 */
  secret?: boolean;
}

export interface ServiceTemplate {
  id: string;
  /** quick = manifest 快捷协议；custom = opendal-custom 透传。 */
  kind: "quick" | "custom";
  fields: ServiceField[];
}

const COMMON_FIELDS: ServiceField[] = [
  { key: "root", type: "text", placeholder: "/" },
  { key: "read_only", type: "boolean" },
  { key: "allow_delete", type: "boolean" },
];

export const SERVICE_TEMPLATES: Record<string, ServiceTemplate> = {
  fs: {
    id: "fs",
    kind: "quick",
    fields: [{ key: "root", type: "text", required: true, placeholder: "/path/to/dir" }],
  },
  s3: {
    id: "s3",
    kind: "quick",
    fields: [
      { key: "bucket", type: "text", required: true },
      { key: "endpoint", type: "text", placeholder: "https://s3.amazonaws.com or MinIO endpoint" },
      { key: "region", type: "text", placeholder: "us-east-1" },
      { key: "access_key_id", type: "text", required: true },
      { key: "secret_access_key", type: "password", required: true, secret: true },
      { key: "enable_virtual_host_style", type: "boolean" },
    ],
  },
  webdav: {
    id: "webdav",
    kind: "quick",
    fields: [
      { key: "endpoint", type: "text", required: true, placeholder: "https://dav.example.com/dav" },
      { key: "username", type: "text" },
      { key: "password", type: "password", secret: true },
    ],
  },
  ftp: {
    id: "ftp",
    kind: "quick",
    fields: [
      { key: "endpoint", type: "text", required: true, placeholder: "ftp.example.com:21" },
      { key: "user", type: "text", placeholder: "anonymous" },
      { key: "password", type: "password", secret: true },
    ],
  },
  sftp: {
    id: "sftp",
    kind: "quick",
    fields: [
      { key: "endpoint", type: "text", required: true, placeholder: "user@host or ssh://user@host:22" },
      // OpenDAL 0.57 的 sftp service 仅支持 keyfile 认证（password 无对应
      // builder 键，后端也不转发）——密码型账号走 sftp-native 快捷协议。
      { key: "key", type: "text", required: true, placeholder: "private key content or path" },
      { key: "known_hosts_strategy", type: "select", options: ["Tolerate", "Strict", "Trust"] },
    ],
  },
  // smb：OpenDAL 自定义 Access 适配层（非编译期 opendal service），因此不进
  // CUSTOM_SERVICES/CUSTOM_SERVICE_SCHEMAS；字段 key 即 sidecar 配置键。
  // password 由宿主 secret binding 收集、domain 由 lifecycle secret 合并进
  // sidecar，均只在内存组装 SmbBuilder，不落盘、不进日志（见 IMPL_PLAN_SMB §2.4）。
  smb: {
    id: "smb",
    kind: "quick",
    fields: [
      { key: "endpoint", type: "text", required: true, placeholder: "nas.local:445 or smb://nas.local" },
      { key: "share", type: "text", placeholder: "share or share/sub/path (optional; blank lists shares)" },
      { key: "username", type: "text" },
      { key: "password", type: "password", secret: true },
      { key: "domain", type: "text", placeholder: "NTLM domain or workgroup" },
    ],
  },
  // sftp-native（双栈决策 2026-08-31）：russh + russh-sftp 自研 Access 适配层
  // （与 smb 同类，不进 CUSTOM_SERVICES/CUSTOM_SERVICE_SCHEMAS）。密码认证
  // 由宿主 secret binding 收集、仅在内存组装 Builder；OpenDAL sftp service
  // 不支持 password，密码型账号（企业 MFT）走这里。置尾与后端 PROTOCOLS
  // 顺序保持一致。
  "sftp-native": {
    id: "sftp-native",
    kind: "quick",
    fields: [
      { key: "endpoint", type: "text", required: true, placeholder: "user@host:22 or ssh://user@host:22" },
      { key: "password", type: "password", secret: true },
      { key: "key", type: "text", placeholder: "private key content or path (optional)" },
      { key: "known_hosts_strategy", type: "select", options: ["Tolerate", "Strict", "Trust"] },
    ],
  },
};

/**
 * opendal-custom 可选服务（产品决策 2026-08-29，参照 OpenDAL services 分类）：
 * 只收 **文件存储** 与 **云对象存储**，db/cache/memory 类不进配置面。
 * backend feature 白名单仍编译 services-memory 供 smoke/单测使用，但不出现在
 * 这里；后端已编译而未列出的服务经 JSON 模式仍可用（未知服务降级为纯 JSON）。
 */
export const CUSTOM_SERVICES: ReadonlySet<string> = new Set([
  "fs",
  "s3",
  "webdav",
  "ftp",
  "sftp",
  // 注意：smb 是自定义 Access 适配层，不是编译期 OpenDAL service，不在此列。
  "gcs",
  "azblob",
  "oss",
  "obs",
  "cos",
]);

/** 各服务常用的配置键提示（JSON 编辑器的占位示例）。 */
export const CUSTOM_CONFIG_HINTS: Readonly<Record<string, string>> = {
  memory: "{}",
  fs: '{ "root": "/tmp/data" }',
  s3: '{\n  "bucket": "bucket",\n  "endpoint": "http://127.0.0.1:9000",\n  "access_key_id": "...",\n  "secret_access_key": "..."\n}',
  gcs: '{ "bucket": "bucket", "credential": "..." }',
  azblob: '{ "container": "container", "account_name": "...", "account_key": "..." }',
  oss: '{ "bucket": "bucket", "endpoint": "https://oss-cn-xxx.aliyuncs.com", "access_key_id": "...", "access_key_secret": "..." }',
  webdav: '{ "endpoint": "https://dav.example.com/dav", "username": "...", "password": "..." }',
  ftp: '{ "endpoint": "ftp.example.com:21" }',
  sftp: '{ "endpoint": "ssh://user@host:22", "key": "~/.ssh/id_ed25519" }',
};

export function quickProtocolIds(): string[] {
  return Object.values(SERVICE_TEMPLATES)
    .filter((template) => template.kind === "quick")
    .map((template) => template.id);
}

export function templateFor(protocol: string): ServiceTemplate | undefined {
  return SERVICE_TEMPLATES[protocol];
}

// ---------------------------------------------------------------------------
// opendal-custom 动态表单 schema（A-FILES 后续轮次）
//
// 每个已知服务给出参数元数据（key/类型/必填/placeholder/安全类别），驱动
// CustomConfigEditor 的 Form Tab；未知服务拿不到 schema → 仅保留 JSON 模式。
// key 即 OpenDAL Builder 配置键，序列化时原样透传，后端零翻译。
// ---------------------------------------------------------------------------

export type CustomFieldType = "text" | "password" | "number" | "boolean" | "select";

/** 安全校验类别：url 仅 http/https；host 拒绝环回/私有/保留地址。 */
export type FieldSecurity = "url" | "host";

/** 字段级校验错误码（组件负责映射成七语文案）。 */
export type FieldError = "required" | "url" | "host" | "number";

export interface CustomFieldSpec {
  key: string;
  type: CustomFieldType;
  required?: boolean;
  placeholder?: string;
  options?: string[];
  /** secret 类字段只入宿主 binding，不进日志。 */
  secret?: boolean;
  security?: FieldSecurity;
}

export type CustomFormValue = string | boolean;

const SFTP_KNOWN_HOSTS: CustomFieldSpec = {
  key: "known_hosts_strategy",
  type: "select",
  options: ["Tolerate", "Strict", "Trust"],
};

/**
 * 已知 opendal-custom 服务的参数 schema（与 backend/Cargo.toml feature 白名单
 * 对齐；字段以 OpenDAL services 文档为准，只收常用键，其余键走 JSON 模式）。
 *
 * 服务范围（产品决策 2026-08-29）：只收 **文件存储**（fs/sftp/ftp/webdav）与
 * **云对象存储**（s3/gcs/azblob/oss/obs/cos）；db/cache/memory 类（redis、
 * memcached、memory、rocksdb…）不属于文件工作台场景，不进配置面（memory 仅
 * 保留为后端 smoke 测试后端，经 JSON 模式仍可用于开发调试）。
 */
export const CUSTOM_SERVICE_SCHEMAS: Readonly<Record<string, readonly CustomFieldSpec[]>> = {
  fs: [{ key: "root", type: "text", required: true, placeholder: "/path/to/dir" }],
  s3: [
    { key: "bucket", type: "text", required: true },
    { key: "endpoint", type: "text", security: "url", placeholder: "https://s3.amazonaws.com" },
    { key: "region", type: "text", placeholder: "us-east-1" },
    { key: "access_key_id", type: "text", required: true },
    { key: "secret_access_key", type: "password", required: true, secret: true },
    { key: "enable_virtual_host_style", type: "boolean" },
  ],
  webdav: [
    { key: "endpoint", type: "text", required: true, security: "url", placeholder: "https://dav.example.com/dav" },
    { key: "username", type: "text" },
    { key: "password", type: "password", secret: true },
  ],
  ftp: [
    { key: "endpoint", type: "text", required: true, security: "host", placeholder: "ftp.example.com:21" },
    { key: "user", type: "text", placeholder: "anonymous" },
    { key: "password", type: "password", secret: true },
  ],
  sftp: [
    { key: "endpoint", type: "text", required: true, security: "host", placeholder: "user@host or ssh://user@host:22" },
    // sftp service 是 keyfile-only：不收 password（OpenDAL 无此 builder 键）。
    { key: "key", type: "text", required: true, placeholder: "private key content or path" },
    SFTP_KNOWN_HOSTS,
  ],
  gcs: [
    { key: "bucket", type: "text", required: true },
    { key: "credential", type: "password", required: true, secret: true, placeholder: "service account JSON" },
    { key: "scope", type: "text", placeholder: "https://www.googleapis.com/auth/devstorage.read_only" },
  ],
  azblob: [
    { key: "container", type: "text", required: true },
    { key: "account_name", type: "text", required: true },
    { key: "account_key", type: "password", secret: true },
    { key: "endpoint", type: "text", security: "url", placeholder: "https://account.blob.core.windows.net" },
  ],
  oss: [
    { key: "bucket", type: "text", required: true },
    { key: "endpoint", type: "text", required: true, security: "url", placeholder: "https://oss-cn-xxx.aliyuncs.com" },
    { key: "access_key_id", type: "text", required: true },
    { key: "access_key_secret", type: "password", required: true, secret: true },
  ],
  obs: [
    { key: "bucket", type: "text", required: true },
    { key: "endpoint", type: "text", required: true, security: "url", placeholder: "https://obs.cn-north-4.myhuaweicloud.com" },
    { key: "access_key_id", type: "text", required: true },
    { key: "secret_access_key", type: "password", required: true, secret: true },
  ],
  cos: [
    { key: "bucket", type: "text", required: true },
    { key: "endpoint", type: "text", security: "url", placeholder: "https://cos.ap-guangzhou.myqcloud.com" },
    { key: "secret_id", type: "text", required: true },
    { key: "secret_key", type: "password", required: true, secret: true },
  ],
};

/** 拿不到 schema 即未知服务：编辑器只保留纯 JSON 模式。 */
export function schemaForService(service: string): readonly CustomFieldSpec[] | undefined {
  return CUSTOM_SERVICE_SCHEMAS[service];
}

// ---------------------------------------------------------------------------
// 安全校验：URL 类参数只允许 http/https；host 类参数拒绝 localhost/环回/
// 私有/保留地址（SSRF 护栏，作用于 UI 层校验，不改后端透传语义）。
// ---------------------------------------------------------------------------

const PRIVATE_V4 = (octets: readonly number[]): boolean =>
  octets[0] === 10 ||
  octets[0] === 127 ||
  (octets[0] === 172 && octets[1] >= 16 && octets[1] <= 31) ||
  (octets[0] === 192 && octets[1] === 168) ||
  (octets[0] === 169 && octets[1] === 254) ||
  (octets[0] === 100 && octets[1] >= 64 && octets[1] <= 127) ||
  (octets[0] === 0);

const PRIVATE_V6 = (host: string): boolean => {
  const value = host.toLowerCase();
  return (
    value === "::" ||
    value === "::1" ||
    value.startsWith("fc") ||
    value.startsWith("fd") ||
    value.startsWith("fe8") ||
    value.startsWith("fe9") ||
    value.startsWith("fea") ||
    value.startsWith("feb")
  );
};

/** 提取 host 类参数的主机名：容忍 scheme://、user@、:port、/path 与 IPv6 括号。 */
export function hostPartOf(rawEndpoint: string): string {
  let value = rawEndpoint.trim();
  const scheme = value.indexOf("://");
  if (scheme >= 0) value = value.slice(scheme + 3);
  const at = value.lastIndexOf("@");
  if (at >= 0) value = value.slice(at + 1);
  if (value.startsWith("[")) {
    const end = value.indexOf("]");
    return end >= 0 ? value.slice(1, end) : value.slice(1);
  }
  const slash = value.indexOf("/");
  if (slash >= 0) value = value.slice(0, slash);
  const colon = value.indexOf(":");
  if (colon >= 0) value = value.slice(0, colon);
  return value;
}

/**
 * host 类字段校验：空主机、localhost、0.0.0.0、环回/私有/链路本地/保留网段
 * 一律拒绝；返回错误码或 null。
 */
export function validateHostField(rawEndpoint: string): FieldError | null {
  const host = hostPartOf(rawEndpoint).toLowerCase();
  if (!host) return "required";
  if (host === "localhost" || host.endsWith(".localhost") || host === "0.0.0.0") return "host";
  if (host.includes(":")) return PRIVATE_V6(host) ? "host" : null;
  if (!/^\d{1,3}(\.\d{1,3}){3}$/.test(host)) return null; // 域名 → 交给 DNS 侧策略
  const octets = host.split(".").map(Number);
  if (octets.some((value) => value > 255)) return "host";
  return PRIVATE_V4(octets) ? "host" : null;
}

/** URL 类字段校验：必须能解析为 http/https 绝对地址，且主机名通过 host 校验。 */
export function validateUrlField(rawUrl: string): FieldError | null {
  const value = rawUrl.trim();
  if (!value) return "required";
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    return "url";
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return "url";
  return validateHostField(parsed.host);
}

/** 单字段校验入口：required → 类型 → 安全类别。 */
export function validateFieldInput(spec: CustomFieldSpec, value: CustomFormValue | undefined): FieldError | null {
  if (spec.type === "boolean") return null;
  const text = typeof value === "string" ? value.trim() : value === undefined || value === null ? "" : String(value);
  if (!text) return spec.required ? "required" : null;
  if (spec.type === "number" && !/^-?\d+(\.\d+)?$/.test(text)) return "number";
  if (spec.security === "url") return validateUrlField(text);
  if (spec.security === "host") return validateHostField(text);
  return null;
}

/** 整份配置按 schema 校验：key → 错误码；仅覆盖 schema 已知字段。 */
export function validateConfigAgainstSchema(
  schema: readonly CustomFieldSpec[],
  config: Record<string, unknown>,
): Record<string, FieldError> {
  const errors: Record<string, FieldError> = {};
  for (const spec of schema) {
    const raw = config[spec.key];
    const value: CustomFormValue | undefined =
      spec.type === "boolean" ? Boolean(raw) : typeof raw === "string" ? raw : raw === undefined || raw === null ? undefined : String(raw);
    const error = validateFieldInput(spec, value);
    if (error) errors[spec.key] = error;
  }
  return errors;
}

// ---------------------------------------------------------------------------
// 表单 ⇄ JSON 双向同步：schema 外的键（含 root）作为 extras 原样保留，
// 序列化时合并回去，保证来回切换不丢自定义键。
// ---------------------------------------------------------------------------

export interface FormHydration {
  values: Record<string, CustomFormValue>;
  extras: Record<string, unknown>;
}

/** 合法 JSON → 表单模型；schema 外键进 extras。 */
export function formValuesFromConfig(
  schema: readonly CustomFieldSpec[],
  config: Record<string, unknown>,
): FormHydration {
  const values: Record<string, CustomFormValue> = {};
  for (const spec of schema) {
    const raw = config[spec.key];
    if (spec.type === "boolean") {
      values[spec.key] = Boolean(raw);
    } else if (raw === undefined || raw === null) {
      values[spec.key] = "";
    } else {
      values[spec.key] = String(raw);
    }
  }
  const extras: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(config)) {
    if (!schema.some((spec) => spec.key === key)) extras[key] = value;
  }
  return { values, extras };
}

/** 表单模型 → JSON 配置：空串/false 不写回，extras 原样合并。 */
export function configFromFormValues(
  schema: readonly CustomFieldSpec[],
  values: Record<string, CustomFormValue>,
  extras: Record<string, unknown>,
): Record<string, unknown> {
  const config: Record<string, unknown> = { ...extras };
  for (const spec of schema) {
    const value = values[spec.key];
    if (spec.type === "boolean") {
      if (value === true) config[spec.key] = true;
      continue;
    }
    if (typeof value === "string" && value.trim()) config[spec.key] = value.trim();
  }
  return config;
}

/** 快捷协议表单 → Builder 配置 map（secret 由宿主 binding 处理，这里原样透出）。 */
export function buildExternalConfig(protocol: string, values: Record<string, string | boolean>): Record<string, unknown> {
  const template = SERVICE_TEMPLATES[protocol];
  if (!template) {
    // opendal-custom：service + config JSON 透传
    let config: Record<string, unknown> = {};
    const rawConfig = values.config;
    if (typeof rawConfig === "string" && rawConfig.trim()) {
      try {
        config = JSON.parse(rawConfig) as Record<string, unknown>;
      } catch {
        config = {};
      }
    }
    return { protocol: "opendal-custom", service: String(values.service ?? ""), config };
  }
  const config: Record<string, unknown> = { protocol };
  for (const field of template.fields) {
    const value = values[field.key];
    if (value === undefined || value === "") continue;
    config[field.key] = field.type === "boolean" ? Boolean(value) : String(value);
  }
  for (const field of COMMON_FIELDS) {
    const value = values[field.key];
    if (value === undefined || value === "") continue;
    config[field.key] = field.type === "boolean" ? Boolean(value) : String(value);
  }
  return config;
}
