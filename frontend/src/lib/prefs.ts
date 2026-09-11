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
  sideTab: SideTab;
  sideCollapsed: boolean;
}

export const UI_PREFS_KEY = "dbx-files.ui";

function sanitize(raw: unknown): Partial<UiPrefs> {
  if (!raw || typeof raw !== "object") return {};
  const value = raw as Record<string, unknown>;
  const prefs: Partial<UiPrefs> = {};
  if (value.sideTab === "tree" || value.sideTab === "quick") prefs.sideTab = value.sideTab;
  if (typeof value.sideCollapsed === "boolean") prefs.sideCollapsed = value.sideCollapsed;
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
  if (!raw) return { sort: { ...DEFAULT_SORT }, sideTab: "tree", sideCollapsed: false };
  const prefs = sanitize(safeParse(raw));
  return {
    sort: prefs.sort ?? { ...DEFAULT_SORT },
    sideTab: prefs.sideTab ?? "tree",
    sideCollapsed: prefs.sideCollapsed ?? false,
  };
}

export function saveUiPrefs(prefs: UiPrefs, storage?: Storage): void {
  try {
    (storage ?? window.localStorage).setItem(UI_PREFS_KEY, JSON.stringify(prefs));
  } catch {
    /* 沙箱/隐私模式：偏好不可持久化，静默忽略 */
  }
}

function safeParse(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}
