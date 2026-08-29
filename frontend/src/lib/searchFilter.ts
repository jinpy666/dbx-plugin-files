// 文件名过滤（对齐 tiny-rdm FileBrowserPane 的过滤语义）：大小写不敏感
// 子串匹配，仅作用于当前目录条目的展示，不改变选择与排序。
import type { FileEntry } from "./api";

export function filterEntries(entries: readonly FileEntry[], query: string): FileEntry[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [...entries];
  return entries.filter((entry) => entry.name.toLowerCase().includes(needle));
}
