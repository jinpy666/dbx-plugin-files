/**
 * 从原生 paste 事件提取文件（Finder/Explorer「复制文件 → Ctrl/Cmd+V」粘贴
 * 上传，对标 ssh 插件 clipboardFiles）。不依赖宿主专属剪贴板 API：常规粘贴
 * 填充 `files`；只暴露 DataTransferItem 的环境（粘贴截图等）走 items 回退。
 */
export interface ClipboardFileItemLike {
  kind?: string;
  getAsFile?: () => File | null;
}

export interface ClipboardFileDataLike {
  files?: ArrayLike<File> | null;
  items?: ArrayLike<ClipboardFileItemLike> | null;
}

export function filesFromClipboard(data: ClipboardFileDataLike | null | undefined): File[] {
  const files = data?.files ? Array.from(data.files) : [];
  if (files.length) return files;
  if (!data?.items) return [];

  const pasted: File[] = [];
  for (let index = 0; index < data.items.length; index += 1) {
    const item = data.items[index];
    if (item?.kind !== "file") continue;
    const file = item.getAsFile?.();
    if (file) pasted.push(file);
  }
  return pasted;
}
