// 快速目录（tiny-rdm quick paths 对标，§8.1）：key 集合与后端
// files/quickPaths 的候选（ops.rs fs_quick_path_candidates）对齐，
// 归一化逻辑供工作台路径栏下拉（QuickPathsMenu）复用。

export interface QuickPath {
  key: string;
  path: string;
}

/** key → i18n 文案键；未知 key 视为契约外条目。 */
const QUICK_PATH_LABEL_KEYS: Record<string, string> = {
  root: "quickRoot",
  home: "quickHome",
  desktop: "quickDesktop",
  downloads: "quickDownloads",
  documents: "quickDocuments",
  pictures: "quickPictures",
};

export function quickPathLabelKey(key: string): string {
  return QUICK_PATH_LABEL_KEYS[key] ?? "quickRoot";
}

/** 过滤未知 key 与空路径：后端契约外条目一律丢弃，展示保持稳定。 */
export function normalizeQuickPaths(paths: QuickPath[] | undefined | null): QuickPath[] {
  return (paths ?? []).filter((item) => Boolean(item.path) && Boolean(QUICK_PATH_LABEL_KEYS[item.key]));
}
