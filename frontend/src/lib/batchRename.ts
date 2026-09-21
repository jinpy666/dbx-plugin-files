// 批量重命名计划计算（纯函数，BatchRenameDrawer 消费/单测直测）。
// 变换次序：查找替换（普通/正则/忽略大小写）→ 前缀 → 后缀 → 序号后缀（-001 式）。

import type { FileEntry } from "./api";

export interface RenamePlanOptions {
  find: string;
  replace: string;
  regex: boolean;
  ignoreCase: boolean;
  prefix: string;
  suffix: string;
  numbering: boolean;
  numberingStart: number;
}

export type RenamePlanError = "" | "batchRenameErrorEmpty" | "batchRenameErrorDuplicate" | "batchRenameErrorExists" | "batchRenameErrorRegex";

export interface RenamePlanRow {
  entry: FileEntry;
  oldName: string;
  newName: string;
  /** 名称有变化（将被应用，除非带错误）。 */
  changed: boolean;
  /** 行级错误 key（组件侧经 t() 渲染）；空串 = 无错。 */
  error: RenamePlanError;
}

export interface RenamePlan {
  rows: RenamePlanRow[];
  /** find 为非法正则：整体提示，行级不再逐行标错。 */
  invalidRegex: boolean;
  /** 将被应用的行数（有变化且无行级错误）。 */
  applicable: number;
}

function isFiniteInteger(value: number): boolean {
  return Number.isFinite(value) && Number.isInteger(value);
}

/** 名称变换：查找替换 + 前缀/后缀 + 序号后缀（调用方负责递增 numberingStart）。 */
export function transformName(name: string, options: RenamePlanOptions, sequence: number): string {
  let next = name;
  if (options.find) {
    if (options.regex) {
      next = next.replace(new RegExp(options.find, `g${options.ignoreCase ? "i" : ""}`), () => options.replace);
    } else if (options.ignoreCase) {
      // 忽略大小写的字面替换：小写串上定位命中段，原文按段拼接（保留原大小写）。
      const lowerName = next.toLowerCase();
      const lowerFind = options.find.toLowerCase();
      let out = "";
      let cursor = 0;
      for (;;) {
        const hit = lowerName.indexOf(lowerFind, cursor);
        if (hit < 0) break;
        out += `${next.slice(cursor, hit)}${options.replace}`;
        cursor = hit + options.find.length;
      }
      next = out + next.slice(cursor);
    } else {
      next = next.split(options.find).join(options.replace);
    }
  }
  let result = `${options.prefix}${next}${options.suffix}`;
  if (options.numbering) {
    // 序号整体追加：-001 式三位零填充；非法起始值按 0 处理。
    const start = isFiniteInteger(sequence) ? sequence : 0;
    result += `-${String(Math.max(0, start)).padStart(3, "0")}`;
  }
  return result;
}

/**
 * 由已加载条目生成重命名计划。错误判定：
 * - 新名为空 → empty；
 * - 与另一行的计划名称重复（按计划顺序先到先得）→ duplicate；
 * - 目录中已存在同名项（对「选中集 + 目录全名」预检，名称有变化才判）→ exists。
 */
export function buildRenamePlan(entries: readonly FileEntry[], options: RenamePlanOptions, siblingNames: readonly string[] = []): RenamePlan {
  const invalidRegex = Boolean(options.regex && options.find && !validRegex(options.find));
  const plannedNames = new Set<string>();
  const existingNames = new Set([...entries.map((entry) => entry.name), ...siblingNames]);
  const rows: RenamePlanRow[] = [];
  let applicable = 0;
  let sequence = isFiniteInteger(Math.trunc(options.numberingStart)) ? Math.trunc(options.numberingStart) : 0;
  for (const entry of entries) {
    const newName = invalidRegex ? entry.name : transformName(entry.name, options, sequence);
    if (options.numbering) sequence += 1;
    const changed = newName !== entry.name;
    let error: RenamePlanError = "";
    if (!invalidRegex) {
      if (!newName) error = "batchRenameErrorEmpty";
      else if (plannedNames.has(newName)) error = "batchRenameErrorDuplicate";
      else if (changed && existingNames.has(newName)) error = "batchRenameErrorExists";
    }
    if (!error) plannedNames.add(newName);
    if (changed && !error) applicable += 1;
    rows.push({ entry, oldName: entry.name, newName, changed, error });
  }
  return { rows, invalidRegex, applicable };
}

export function validRegex(pattern: string): boolean {
  try {
    new RegExp(pattern);
    return true;
  } catch {
    return false;
  }
}
