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
// 目录）。目录字符串由用户手输（无系统目录选择器），下发前只做 trim。

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

function safeParse(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}
