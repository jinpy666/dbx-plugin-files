// 快速目录（tiny-rdm quick paths 对标，§8.1）：key 集合与后端
// files/quickPaths 的候选（ops.rs fs_quick_path_candidates）对齐，
// 归一化逻辑供工作台快速定位侧栏（QuickSidebar）复用。
import type { Component } from "vue";
import { Download, FileText, HardDrive, Home, Image as ImageIcon, Monitor } from "@lucide/vue";

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

/** key → 图标（快速定位侧栏共用）；未知 key 回退硬盘图标。 */
export function quickPathIcon(key: string): Component {
  switch (key) {
    case "home":
      return Home;
    case "desktop":
      return Monitor;
    case "downloads":
      return Download;
    case "documents":
      return FileText;
    case "pictures":
      return ImageIcon;
    default:
      return HardDrive;
  }
}

export function quickPathLabelKey(key: string): string {
  return QUICK_PATH_LABEL_KEYS[key] ?? "quickRoot";
}

/** 过滤未知 key 与空路径：后端契约外条目一律丢弃，展示保持稳定。 */
export function normalizeQuickPaths(paths: QuickPath[] | undefined | null): QuickPath[] {
  return (paths ?? []).filter((item) => Boolean(item.path) && Boolean(QUICK_PATH_LABEL_KEYS[item.key]));
}
