/**
 * 传输同名冲突策略（对标 ssh 插件 downloadConflictPolicy 三档，但 Files 里
 * 同时作用于上传与下载落盘）：
 * - "ask"（默认）：预检发现撞名时弹「覆盖 / 自动重命名 / 取消」三键对话框；
 * - "rename"：自动让位为 `name (n).ext`（浏览器风格，1..999，之后时间戳兜底）；
 * - "overwrite"：直接替换同名文件。
 */

import { nameCollisionKey } from "./nameCollision";

export type ConflictPolicy = "ask" | "rename" | "overwrite";

export const CONFLICT_POLICIES: readonly ConflictPolicy[] = ["ask", "rename", "overwrite"];

export function sanitizeConflictPolicy(value: unknown): ConflictPolicy {
  return value === "rename" || value === "overwrite" ? value : "ask";
}

/** 上传队列项：名字与内容源解耦（rename 时改 name 不动内容源）。 */
export interface UploadQueueItem {
  name: string;
  /** 上传目标内的相对目录（POSIX 风格，无首尾斜杠；缺省 = 目标根）。
   * 文件夹上传按 webkitRelativePath 还原，普通上传不设。 */
  remoteDir?: string;
  size: number;
  readChunk: (offset: number, length: number) => Promise<Uint8Array>;
}

/** 按 `remoteDir` 分组（预检每个子目录各自对目标清单查撞名；改名不跨目录）。 */
export function groupByRemoteDir<T extends UploadQueueItem>(items: readonly T[]): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const key = item.remoteDir ?? "";
    const group = groups.get(key);
    if (group) group.push(item);
    else groups.set(key, [item]);
  }
  return groups;
}

/**
 * 取 `name` 在 `taken`（已占用名字的**归一化键**集合，见 nameCollisionKey）
 * 之外的第一个可用名，形如 `report (1).pdf`；扩展名前插入，无扩展名/点文件
 * 保持整体作 stem。找到的新名字会以键写回 `taken`，同批多个改名互不重复
 * （含大小写/NFC 变体互撞）。
 */
export function nextAvailableName(name: string, taken: Set<string>): string {
  const dot = name.lastIndexOf(".");
  const split = dot > 0 ? dot : name.length;
  const stem = name.slice(0, split);
  const ext = name.slice(split);
  for (let index = 1; index <= 999; index += 1) {
    const candidate = `${stem} (${index})${ext}`;
    if (!taken.has(nameCollisionKey(candidate))) {
      taken.add(nameCollisionKey(candidate));
      return candidate;
    }
  }
  const stamp = Date.now();
  const candidate = `${stem}-${stamp}${ext}`;
  taken.add(nameCollisionKey(candidate));
  return candidate;
}

/**
 * 冲突解析（上传方向）：`existing` 是目标目录已有名字集合。rename/overwrite
 * 档纯本地计算；ask 档由调用方先弹窗再传 mode 进来（undefined = 用户取消，
 * 整批放弃）。同批文件之间的名字互撞也一并规避。撞名判定按 nameCollisionKey
 * 归一化：大小写不敏感目标上 `Report.pdf` 撞 `Report.PDF` 是覆盖，rename 档
 * 必须让位（taken 集合内保存的是归一化键）。
 */
export function resolveUploadNames<T extends UploadQueueItem>(
  items: readonly T[],
  existing: ReadonlySet<string>,
  mode: "rename" | "overwrite",
): T[] {
  if (mode === "overwrite") return [...items];
  // taken = 已有名字 ∪ 同批原名（键形态）：改名候选既不撞目录里其他文件，
  // 也不撞同批另一个文件的原名——含大小写/NFC 变体。冲突判定则对「目录
  // 已有 + 批内已出现」两个键集：批内大小写变体在不敏感目标上互为覆盖，
  // 后到者同样要让位。
  const existingKeys = new Set([...existing].map(nameCollisionKey));
  const taken = new Set<string>([...existing, ...items.map((item) => item.name)].map(nameCollisionKey));
  const seen = new Set<string>();
  return items.map((item) => {
    const key = nameCollisionKey(item.name);
    if (!existingKeys.has(key) && !seen.has(key)) {
      seen.add(key);
      return item;
    }
    const renamed = nextAvailableName(item.name, taken);
    seen.add(nameCollisionKey(renamed));
    return { ...item, name: renamed };
  });
}
