// 预览/编辑辅助（A-FILES ②）：读上限、写上限、图片 MIME、十六进制 dump。
// files/read ≤2MiB（后端契约），files/write 解码 ≤4MiB（超限引导走上传）。

export const READ_MAX_BYTES = 2 * 1024 * 1024;
export const WRITE_MAX_BYTES = 4 * 1024 * 1024;
// 分块预览的硬上限（files/readRange 流式拼装，单文件 ≤256MiB 才进浏览器内存）。
export const PREVIEW_MAX_BYTES = 256 * 1024 * 1024;

/** 编辑保存前的大小判断：序列化后字节数 ≤ files/write 上限才允许直接写回。 */
export function canEditBytes(byteLength: number): boolean {
  return byteLength >= 0 && byteLength <= WRITE_MAX_BYTES;
}

const IMAGE_MIMES: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  bmp: "image/bmp",
  svg: "image/svg+xml",
};

/** 按扩展名取图片 MIME；非图片扩展名返回 null。 */
export function imageMimeFor(path: string): string | null {
  const index = path.lastIndexOf(".");
  if (index < 0) return null;
  return IMAGE_MIMES[path.slice(index + 1).toLowerCase()] ?? null;
}

function hexByte(byte: number): string {
  return byte.toString(16).padStart(2, "0");
}

function asciiByte(byte: number): string {
  return byte >= 0x20 && byte <= 0x7e ? String.fromCharCode(byte) : ".";
}

/**
 * 十六进制 dump（A-FILES ②c）：每行 16 字节 —— 偏移(8位hex) + hex 两列 + ASCII。
 * 仅渲染传入字节（调用方负责截取前 512B）。
 */
export function hexDump(bytes: Uint8Array, bytesPerRow = 16): string {
  const lines: string[] = [];
  for (let offset = 0; offset < bytes.length; offset += bytesPerRow) {
    const row = bytes.subarray(offset, Math.min(offset + bytesPerRow, bytes.length));
    const hexParts: string[] = [];
    const ascii: string[] = [];
    for (const byte of row) {
      hexParts.push(hexByte(byte));
      ascii.push(asciiByte(byte));
    }
    while (hexParts.length < bytesPerRow) hexParts.push("  ");
    const mid = bytesPerRow / 2;
    const hex = `${hexParts.slice(0, mid).join(" ")}  ${hexParts.slice(mid).join(" ")}`;
    lines.push(`${offset.toString(16).padStart(8, "0")}  ${hex}  |${ascii.join("")}|`);
  }
  return lines.join("\n");
}
