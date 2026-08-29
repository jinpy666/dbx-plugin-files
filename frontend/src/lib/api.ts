// sidecar 调用封装：自动注入 connectionId；错误统一转成可读消息。
// 约 定：所有 files/* 方法只带 connectionId（Files 无 session 概念）。

export interface FileEntry {
  name: string;
  path: string;
  kind: "file" | "directory";
  size?: number;
  modifiedAt?: string;
}

/**
 * 后端 `FileEntry.kind` 序列化为 "dir"|"file"（model.rs），前端统一用
 * "directory"。P-FILES ④ 浏览器验证发现的真实契约错位：不做归一化时
 * 目录双击/目录判断（排序、删除递归、右键菜单）全部失效。
 */
export function normalizeEntry(raw: FileEntry): FileEntry {
  return (raw as { kind: string }).kind === "dir" ? { ...raw, kind: "directory" } : raw;
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
