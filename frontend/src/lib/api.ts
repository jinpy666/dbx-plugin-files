// sidecar 调用封装：自动注入 connectionId；错误统一转成可读消息。
// 约 定：所有 files/* 方法只带 connectionId（Files 无 session 概念）。

export interface FileEntry {
  name: string;
  path: string;
  kind: "file" | "directory";
  size?: number;
  modifiedAt?: string;
  /** FTP 显示解码名（charset.ts；仅显示用，操作一律用 name/path）。 */
  displayName?: string;
}

/**
 * 后端 `FileEntry.kind` 序列化为 "dir"|"file"（model.rs），前端统一用
 * "directory"。P-FILES ④ 浏览器验证发现的真实契约错位：不做归一化时
 * 目录双击/目录判断（排序、删除递归、右键菜单）全部失效。
 *
 * `modifiedAt` 同类错位：后端序列化为 u64 epoch 毫秒数字，而前端契约是
 * ISO 字符串；不归一化时按「修改时间」排序会对数字调 localeCompare 崩溃
 * （`?? ""` 兜不住非 nullish 的数字）。mock 宿主给的是 ISO 字符串，掩盖了
 * 该错位。这里统一归一为 ISO 字符串，使声明的类型为真。
 */
export function normalizeEntry(raw: FileEntry): FileEntry {
  const entry: FileEntry = (raw as { kind: string }).kind === "dir" ? { ...raw, kind: "directory" } : raw;
  if (typeof entry.modifiedAt === "number") {
    return { ...entry, modifiedAt: new Date(entry.modifiedAt).toISOString() };
  }
  return entry;
}

export function normalizeEntries(entries: readonly FileEntry[]): FileEntry[] {
  return entries.map(normalizeEntry);
}

export interface FileCapabilities {
  copy: boolean;
  rename: boolean;
  presign: boolean;
  write: boolean;
  delete?: boolean;
  move?: boolean;
  [key: string]: boolean | undefined;
}

export class FilesApiError extends Error {
  readonly method: string;
  constructor(method: string, message: string) {
    super(message);
    this.method = method;
  }
}

export function isMethodMissing(error: unknown): boolean {
  return /method not found/i.test(error instanceof Error ? error.message : String(error));
}

/**
 * 宿主桥（window.dbxPlugin.request）缺方法：旧宿主桥对未知 host 方法报
 * "Unsupported plugin host method '<method>'"，桥在但回调缺席时报
 * "… is unavailable"。用于 host.* 能力探测（如 host.reopenConnection）
 * 的旧宿主降级判定；sidecar 方法的同类判定见 isMethodMissing。
 */
export function isHostMethodMissing(error: unknown): boolean {
  const message = errorMessage(error);
  return /unsupported plugin host method|connection reopen is unavailable/i.test(message) || isMethodMissing(error);
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

type Invoke = <T = unknown>(method: string, params?: unknown, options?: { timeoutMs?: number }) => Promise<T>;

let invokeRef: Invoke | null = null;
let connectionRef: string | null = null;

export function bindApi(invoke: Invoke, connectionId: string | null): void {
  invokeRef = invoke;
  connectionRef = connectionId;
}

export function currentConnectionId(): string | null {
  return connectionRef;
}

/** 调用 sidecar：注入 connectionId（已有则不覆盖）。 */
export async function call<T = Record<string, unknown>>(method: string, params: Record<string, unknown> = {}): Promise<T> {
  if (!invokeRef) throw new FilesApiError(method, "api not bound");
  const payload = { ...params };
  if (method.startsWith("files/") && !method.startsWith("files/transfers") && payload.connectionId === undefined && connectionRef) {
    payload.connectionId = connectionRef;
  }
  try {
    return await invokeRef<T>(method, payload);
  } catch (cause) {
    throw new FilesApiError(method, errorMessage(cause));
  }
}

/** 生命周期类调用（connection/test 等不需要 connectionId 注入）。 */
export async function callLifecycle<T = Record<string, unknown>>(method: string, params: Record<string, unknown> = {}): Promise<T> {
  if (!invokeRef) throw new FilesApiError(method, "api not bound");
  try {
    return await invokeRef<T>(method, params);
  } catch (cause) {
    throw new FilesApiError(method, errorMessage(cause));
  }
}

// ---- path helpers ---------------------------------------------------------

export function joinPath(base: string, name: string): string {
  const prefix = base.endsWith("/") ? base : `${base}/`;
  return `${prefix}${name}`;
}

export function parentPath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  if (index <= 0) return "/";
  return trimmed.slice(0, index);
}

export function baseName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1) || "/";
}

export function formatBytes(size: number | undefined): string {
  if (size === undefined || Number.isNaN(size)) return "";
  if (size < 1024) return `${size} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = size;
  let unit = "B";
  for (const next of units) {
    if (value < 1024) break;
    value /= 1024;
    unit = next;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${unit}`;
}

export function formatTime(value: string | undefined): string {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  const pad = (input: number) => String(input).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
