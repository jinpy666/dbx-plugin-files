/**
 * 传输同名冲突策略（对标 ssh 插件 downloadConflictPolicy 三档，但 Files 里
 * 同时作用于上传与下载落盘）：
 * - "ask"（默认）：预检发现撞名时弹「覆盖 / 自动重命名 / 取消」三键对话框；
 * - "rename"：自动让位为 `name (n).ext`（浏览器风格，1..999，之后时间戳兜底）；
 * - "overwrite"：直接替换同名文件。
 */

export type ConflictPolicy = "ask" | "rename" | "overwrite";

export const CONFLICT_POLICIES: readonly ConflictPolicy[] = ["ask", "rename", "overwrite"];

export function sanitizeConflictPolicy(value: unknown): ConflictPolicy {
  return value === "rename" || value === "overwrite" ? value : "ask";
}

/** 上传队列项：名字与内容源解耦（rename 时改 name 不动内容源）。 */
export interface UploadQueueItem {
  name: string;
  size: number;
  readChunk: (offset: number, length: number) => Promise<Uint8Array>;
}

/**
 * 取 `name` 在 `taken`（已占用名字集合）之外的第一个可用名，形如
 * `report (1).pdf`；扩展名前插入，无扩展名/点文件保持整体作 stem。找到的
 * 新名字会写回 `taken`，同批多个改名互不重复；与撞名原文件保持原有名字占用。
 */
export function nextAvailableName(name: string, taken: Set<string>): string {
  const dot = name.lastIndexOf(".");
  const split = dot > 0 ? dot : name.length;
  const stem = name.slice(0, split);
  const ext = name.slice(split);
  for (let index = 1; index <= 999; index += 1) {
    const candidate = `${stem} (${index})${ext}`;
    if (!taken.has(candidate)) {
      taken.add(candidate);
      return candidate;
    }
  }
  const stamp = Date.now();
  const candidate = `${stem}-${stamp}${ext}`;
  taken.add(candidate);
  return candidate;
}

/**
 * 冲突解析（上传方向）：`existing` 是目标目录已有名字集合。rename/overwrite
 * 档纯本地计算；ask 档由调用方先弹窗再传 mode 进来（undefined = 用户取消，
 * 整批放弃）。同批文件之间的名字互撞也一并规避。
 */
export function resolveUploadNames<T extends UploadQueueItem>(
  items: readonly T[],
  existing: ReadonlySet<string>,
  mode: "rename" | "overwrite",
): T[] {
  if (mode === "overwrite") return [...items];
  // taken = 已有名字 ∪ 同批原名：既防止改名撞到目录里其他文件，也防止
  // 改成同批另一个文件的原名。
  const taken = new Set<string>(existing);
  for (const item of items) taken.add(item.name);
  return items.map((item) =>
    existing.has(item.name) ? { ...item, name: nextAvailableName(item.name, taken) } : item,
  );
}
