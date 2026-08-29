// 压缩包识别（A-FILES ③）：按扩展名识别 zip / tar 系压缩包。
// 后端 archive 能力（files/archiveList / files/extract）见交接文档
// docs/PROGRESS-A-FILES.zh-CN.md；zip 解压标注为 Phase 2。

export type ArchiveKind = "zip" | "tar";

const ARCHIVE_EXTS: Array<{ suffix: string; kind: ArchiveKind }> = [
  { suffix: ".zip", kind: "zip" },
  { suffix: ".tar.gz", kind: "tar" },
  { suffix: ".tgz", kind: "tar" },
  { suffix: ".tar", kind: "tar" },
];

/** 识别压缩包类型；非压缩包返回 null。大小写不敏感。 */
export function archiveKind(path: string): ArchiveKind | null {
  const lower = path.toLowerCase();
  for (const { suffix, kind } of ARCHIVE_EXTS) {
    if (lower.endsWith(suffix)) return kind;
  }
  return null;
}

export function isArchivePath(path: string): boolean {
  return archiveKind(path) !== null;
}
