// 危险路径确认规则：purge / 递归删除 / syncDir 覆盖目标 / 批量删除。
// 模式对齐 ssh-sftp dangerousCommands.ts：纯函数 + 显式命中原因，供 ConfirmDialog 展示。

export type DangerousLevel = "none" | "warn" | "danger";

export interface DangerousHit {
  id: "purge-root" | "purge" | "recursive-delete" | "sync-overwrite" | "copy-overwrite" | "bulk-delete" | "system-path";
  /** 英文兜底文案（或路径本身）；UI 层按 id 映射 i18n（P2-2），label 仅作回退。 */
  label: string;
  /** 结构化参数：purge/system-path/sync-overwrite/copy-overwrite 携带目标路径。 */
  path?: string;
  /** 结构化参数：bulk-delete 携带条数。 */
  count?: number;
}

export interface DangerousInspection {
  level: DangerousLevel;
  hits: DangerousHit[];
}

const SYSTEM_PATH_RE = /^\/(etc|usr|bin|sbin|boot|proc|sys|dev|windows|system32)(\/|$)/i;
const BULK_DELETE_THRESHOLD = 10;

function normalize(path: string): string {
  return path.replace(/\/+$/, "") || "/";
}

/** purge：根路径直接 danger；系统路径 warn；其余 danger（语义即递归清空）。 */
export function inspectPurge(path: string, root: string): DangerousInspection {
  const target = normalize(path);
  const base = normalize(root || "/");
  const hits: DangerousHit[] = [];
  if (target === "/" || target === base) {
    hits.push({ id: "purge-root", label: "purge root" });
    return { level: "danger", hits };
  }
  if (SYSTEM_PATH_RE.test(target)) hits.push({ id: "system-path", label: target, path: target });
  hits.push({ id: "purge", label: `purge ${target}`, path: target });
  return { level: "danger", hits };
}

/** 删除：目录（递归语义）danger；批量（>10 项）warn；普通文件 none。 */
export function inspectDelete(targets: Array<{ path: string; kind: "file" | "directory" }>): DangerousInspection {
  const hits: DangerousHit[] = [];
  let level: DangerousLevel = "none";
  if (targets.some((target) => target.kind === "directory")) {
    hits.push({ id: "recursive-delete", label: "recursive delete" });
    level = "danger";
  } else if (targets.length > BULK_DELETE_THRESHOLD) {
    hits.push({ id: "bulk-delete", label: `${targets.length} items`, count: targets.length });
    level = "warn";
  }
  return { level, hits };
}

/** syncDir：目标总是可能被覆盖/删除（rclone sync 语义），必须确认。 */
export function inspectSyncDir(targetPath: string): DangerousInspection {
  return { level: "danger", hits: [{ id: "sync-overwrite", label: normalize(targetPath), path: normalize(targetPath) }] };
}

/** copyDir：仅同名覆盖，warn。 */
export function inspectCopyDir(targetPath: string): DangerousInspection {
  return { level: "warn", hits: [{ id: "copy-overwrite", label: normalize(targetPath), path: normalize(targetPath) }] };
}

/** 统一入口：kind 决定规则集。 */
export function inspect(kind: "purge" | "delete" | "syncDir" | "copyDir", payload: {
  path?: string;
  root?: string;
  targets?: Array<{ path: string; kind: "file" | "directory" }>;
}): DangerousInspection {
  switch (kind) {
    case "purge":
      return inspectPurge(payload.path ?? "/", payload.root ?? "");
    case "delete":
      return inspectDelete(payload.targets ?? []);
    case "syncDir":
      return inspectSyncDir(payload.path ?? "/");
    case "copyDir":
      return inspectCopyDir(payload.path ?? "/");
  }
}

/** 是否需要弹确认框（level != none 即需要）。 */
export function requiresConfirm(inspection: DangerousInspection): boolean {
  return inspection.level !== "none";
}
