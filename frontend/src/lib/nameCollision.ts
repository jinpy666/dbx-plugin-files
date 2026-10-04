// 名字碰撞归一化键：大小写不敏感文件系统（APFS/Windows/SMB）上
// `Report.pdf` 与 `Report.PDF` 是同一个文件；macOS NFC 输入 vs APFS NFD
// 存储的变体同理。预检集合一律按键比对（显示名保持原样），否则重命名/
// 上传的「同名预检」会在不敏感目标上静默覆盖已有文件。
export function nameCollisionKey(name: string): string {
  return name.normalize("NFC").toLowerCase();
}
