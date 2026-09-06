// 排序比较器（A-FILES ④c）：目录恒置顶，列内按选择列/方向比较。
// 从 App.vue 抽出为纯函数，便于 vitest 覆盖与排序状态持久化复用。

import type { FileEntry } from "./api";

export type SortColumn = "name" | "size" | "modified";

export interface SortState {
  column: SortColumn;
  direction: "asc" | "desc";
}

export const DEFAULT_SORT: SortState = { column: "name", direction: "asc" };

/** 点击同一列翻转方向，点击新列回到升序。 */
export function toggleSortState(current: SortState, column: SortColumn): SortState {
  if (current.column === column) {
    return { column, direction: current.direction === "asc" ? "desc" : "asc" };
  }
  return { column, direction: "asc" };
}

export function compareEntries(a: FileEntry, b: FileEntry, column: SortColumn, direction: "asc" | "desc"): number {
  // 目录永远排在文件前面（与历史行为一致，不受方向影响）。
  if (a.kind !== b.kind) return a.kind === "directory" ? -1 : 1;
  const sign = direction === "asc" ? 1 : -1;
  if (column === "size") return ((a.size ?? 0) - (b.size ?? 0)) * sign;
  if (column === "modified") return (a.modifiedAt ?? "").localeCompare(b.modifiedAt ?? "") * sign;
  // R3-P2-3：数字感知自然序（a2 < a10，对标 FileZilla/Finder）——日志切片、
  // 分卷等序号文件不再按字典序排成 a10 < a2。
  return a.name.localeCompare(b.name, undefined, { numeric: true }) * sign;
}

export function sortEntries(entries: readonly FileEntry[], state: SortState): FileEntry[] {
  return [...entries].sort((a, b) => compareEntries(a, b, state.column, state.direction));
}
