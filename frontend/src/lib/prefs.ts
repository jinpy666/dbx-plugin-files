// UI 偏好持久化（A-FILES ①/④c）：localStorage 保存布局偏好（排序/侧栏）。
// 只存非敏感 UI 状态；沙箱环境 localStorage 可能不可用，全部 try/catch 兜底。
// 历史版本存过 rightTab（预览已改弹窗）、dualPane（已改为会话内开关），
// sanitize 直接忽略这些未知字段。

import type { SortState } from "./sorting";
import { DEFAULT_SORT } from "./sorting";

/** 侧栏 tab（tree=目录树，quick=快捷目录）；默认 tree。 */
export type SideTab = "tree" | "quick";

export interface UiPrefs {
  sort: SortState;
  leftSideTab: SideTab;
  rightSideTab: SideTab;
  leftSideCollapsed: boolean;
  rightSideCollapsed: boolean;
  /** 预览浮窗尺寸（对齐 rclone-dashboard 的 per-layout 记忆）；缺省走居中默认。 */
  previewWin?: PreviewWin;
  /** 设置弹窗上次停留的分类：重开时回到原地，减少重复点击。 */
  settingsCategory?: "downloads" | "openWith" | "transfer" | "mounts";
  /** 设置弹窗记忆尺寸（对齐预览浮窗的 per-layout 记忆）；缺省走默认大小。 */
  settingsWin?: PreviewWin;
}

export interface PreviewWin {
  width: number;
  height: number;
}

/** 预览浮窗拖拽缩放的上下限：避免缩到不可读或超出宿主视口。 */
export const PREVIEW_MIN = { width: 360, height: 240 };
export const PREVIEW_MAX = { width: 4096, height: 4096 };

function sanitizePreviewWin(raw: unknown): PreviewWin | undefined {
  if (!raw || typeof raw !== "object") return undefined;
  const value = raw as Record<string, unknown>;
  const width = Number(value.width);
  const height = Number(value.height);
  if (!Number.isFinite(width) || !Number.isFinite(height)) return undefined;
  if (width < PREVIEW_MIN.width || height < PREVIEW_MIN.height) return undefined;
  return {
    width: Math.min(Math.round(width), PREVIEW_MAX.width),
    height: Math.min(Math.round(height), PREVIEW_MAX.height),
  };
}

export const UI_PREFS_KEY = "dbx-files.ui";

function sanitize(raw: unknown): Partial<UiPrefs> {
  if (!raw || typeof raw !== "object") return {};
  const value = raw as Record<string, unknown>;
  const prefs: Partial<UiPrefs> = {};
  if (value.leftSideTab === "tree" || value.leftSideTab === "quick") prefs.leftSideTab = value.leftSideTab;
  if (value.rightSideTab === "tree" || value.rightSideTab === "quick") prefs.rightSideTab = value.rightSideTab;
  if (typeof value.leftSideCollapsed === "boolean") prefs.leftSideCollapsed = value.leftSideCollapsed;
  if (typeof value.rightSideCollapsed === "boolean") prefs.rightSideCollapsed = value.rightSideCollapsed;
  // 兼容旧版本的单侧栏偏好：作为两栏初始值，默认左栏仍优先收藏。
  if (value.sideTab === "tree" || value.sideTab === "quick") {
    prefs.leftSideTab ??= value.sideTab;
    prefs.rightSideTab ??= value.sideTab;
  }
  if (typeof value.sideCollapsed === "boolean") {
    prefs.leftSideCollapsed ??= value.sideCollapsed;
    prefs.rightSideCollapsed ??= value.sideCollapsed;
  }
  if (value.sort && typeof value.sort === "object") {
    const sort = value.sort as Record<string, unknown>;
    if (sort.column === "name" || sort.column === "size" || sort.column === "modified") {
      if (sort.direction === "asc" || sort.direction === "desc") {
        prefs.sort = { column: sort.column, direction: sort.direction };
      }
    }
  }
  prefs.previewWin = sanitizePreviewWin(value.previewWin);
  if (
    typeof value.settingsCategory === "string" &&
    ["downloads", "openWith", "transfer", "mounts"].includes(value.settingsCategory)
  ) {
    prefs.settingsCategory = value.settingsCategory as "downloads" | "openWith" | "transfer" | "mounts";
  }
  prefs.settingsWin = sanitizePreviewWin(value.settingsWin);
  return prefs;
}

/** 读取偏好；损坏/缺失字段回退默认值。storage 可注入（测试用）。 */
export function loadUiPrefs(storage?: Storage): UiPrefs {
  let raw: string | null = null;
  try {
    raw = (storage ?? window.localStorage).getItem(UI_PREFS_KEY);
  } catch {
    raw = null;
  }
  if (!raw) return { sort: { ...DEFAULT_SORT }, leftSideTab: "quick", rightSideTab: "tree", leftSideCollapsed: false, rightSideCollapsed: false };
  const prefs = sanitize(safeParse(raw));
  return {
    sort: prefs.sort ?? { ...DEFAULT_SORT },
    leftSideTab: prefs.leftSideTab ?? "quick",
    rightSideTab: prefs.rightSideTab ?? "tree",
    leftSideCollapsed: prefs.leftSideCollapsed ?? false,
    rightSideCollapsed: prefs.rightSideCollapsed ?? false,
    previewWin: prefs.previewWin,
    settingsCategory: ["downloads", "openWith", "transfer", "mounts"].includes(
      prefs.settingsCategory as string,
    )
      ? (prefs.settingsCategory as "downloads" | "openWith" | "transfer" | "mounts")
      : undefined,
    settingsWin: sanitizePreviewWin(prefs.settingsWin),
  };
}

export function saveUiPrefs(prefs: UiPrefs, storage?: Storage): void {
  try {
    (storage ?? window.localStorage).setItem(UI_PREFS_KEY, JSON.stringify(prefs));
  } catch {
    /* 沙箱/隐私模式：偏好不可持久化，静默忽略 */
  }
}

// —— 下载保存目录（对标 ssh 插件 downloadDir 设置）——————————————
// localStorage 持久化用户偏好的落盘目录；空值 = 跟随 sidecar 默认（系统下载
// 目录）。目录字符串由用户手输或系统目录选择器提供；sidecar 在保存和
// 下载前校验。

export const DOWNLOAD_DIR_KEY = "dbx-files.downloadDir";

/** 读取下载目录偏好；未设置/不可用返回空串（跟随默认）。storage 可注入。 */
export function loadDownloadDir(storage?: Storage): string {
  let raw: string | null = null;
  try {
    raw = (storage ?? window.localStorage).getItem(DOWNLOAD_DIR_KEY);
  } catch {
    raw = null;
  }
  return raw?.trim() ?? "";
}

/** 保存下载目录偏好；空串 = 清除偏好（恢复默认目录）。 */
export function persistDownloadDir(value: string, storage?: Storage): void {
  const normalized = value.trim();
  try {
    if (normalized) (storage ?? window.localStorage).setItem(DOWNLOAD_DIR_KEY, normalized);
    else (storage ?? window.localStorage).removeItem(DOWNLOAD_DIR_KEY);
  } catch {
    /* 偏好仅对当前会话生效 */
  }
}

// —— 外部打开应用（issue #11，补齐「默认应用打开」的自定义能力）—————————
// 用户可为下载产物指定外部应用（如 notepad++）打开，覆盖系统默认应用：
// defaultApp 是全局默认可执行文件路径，mappings 按扩展名覆盖（如 ini）。
// 路径在前端只做基础清洗（trim/小写扩展名）；存在性与绝对路径校验由
// sidecar 在保存偏好（files/local/validate-open-app）和打开（files/local/
// open 的 app 参数）时执行。

export const OPEN_APP_KEY = "dbx-files.openApp";

/** 单条扩展名映射：ext 为小写、不含点；app 为可执行文件绝对路径。 */
export interface OpenAppMapping {
  ext: string;
  app: string;
}

export interface OpenAppPrefs {
  /** 全局默认外部应用；空串 = 跟随系统默认应用。 */
  defaultApp: string;
  /** 按扩展名覆盖，命中时优先于 defaultApp。 */
  mappings: OpenAppMapping[];
}

/** 扩展名归一化：去前导点、trim、小写（ini / .INI 都映射到 ini）。 */
function normalizeExt(raw: unknown): string {
  return typeof raw === "string" ? raw.trim().replace(/^\.+/, "").toLowerCase() : "";
}

function sanitizeOpenAppPath(raw: unknown): string {
  return typeof raw === "string" ? raw.trim() : "";
}

function sanitizeOpenAppPrefs(raw: unknown): OpenAppPrefs {
  if (!raw || typeof raw !== "object") return { defaultApp: "", mappings: [] };
  const value = raw as Record<string, unknown>;
  const mappings: OpenAppMapping[] = [];
  if (Array.isArray(value.mappings)) {
    for (const entry of value.mappings) {
      if (!entry || typeof entry !== "object") continue;
      const record = entry as Record<string, unknown>;
      const ext = normalizeExt(record.ext);
      const app = sanitizeOpenAppPath(record.app);
      // 半成品行（只填了扩展名或只填了应用）不落盘。
      if (ext && app) mappings.push({ ext, app });
    }
  }
  return { defaultApp: sanitizeOpenAppPath(value.defaultApp), mappings };
}

/** 读取外部应用偏好；损坏/缺失字段回退默认值（系统默认应用）。storage 可注入。 */
export function loadOpenAppPrefs(storage?: Storage): OpenAppPrefs {
  let raw: string | null = null;
  try {
    raw = (storage ?? window.localStorage).getItem(OPEN_APP_KEY);
  } catch {
    raw = null;
  }
  if (!raw) return { defaultApp: "", mappings: [] };
  return sanitizeOpenAppPrefs(safeParse(raw));
}

/** 保存外部应用偏好；写入前按同一规则清洗（默认 + 空 mappings = 系统默认应用）。 */
export function persistOpenAppPrefs(prefs: OpenAppPrefs, storage?: Storage): void {
  const normalized = sanitizeOpenAppPrefs(prefs);
  try {
    (storage ?? window.localStorage).setItem(OPEN_APP_KEY, JSON.stringify(normalized));
  } catch {
    /* 沙箱/隐私模式：偏好仅对当前会话生效 */
  }
}

/** 解析某文件应使用的外部应用：扩展名映射优先，其次全局默认；空串 = 系统默认应用。 */
export function resolveOpenApp(prefs: OpenAppPrefs, fileName: string): string {
  const dot = fileName.lastIndexOf(".");
  if (dot >= 0 && dot + 1 < fileName.length) {
    const ext = fileName.slice(dot + 1).toLowerCase();
    const mapped = prefs.mappings.find((mapping) => mapping.ext === ext);
    if (mapped) return mapped.app;
  }
  return prefs.defaultApp;
}

function safeParse(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}
