// 上传目标解析（P1-5 上传方向语义）：
// 「上传」= 把本地文件传向远端存储（FileZilla/tiny-rdm 心智）。双栏时右栏
// 恒为远端目标面（右栏连接选项不含本地 __local__），因此双栏固定落到右栏
// 当前目录；单栏时左栏即当前连接本体，落到左栏当前目录（行为不变）。
// 返回 connectionId 与 sideConnectionId 的约定一致：undefined = 当前连接
// （由 api 层默认注入）。

export interface UploadTarget {
  path: string;
  connectionId?: string;
}

export function resolveUploadTarget(input: {
  dualPane: boolean;
  leftPath: string;
  rightPath: string;
  /** 左栏显式连接（双栏默认 __local__；单栏恒 undefined）。 */
  leftConnectionId?: string;
  /** 右栏显式连接（同连接时 undefined）。 */
  rightConnectionId?: string;
}): UploadTarget {
  if (input.dualPane) {
    return { path: input.rightPath, connectionId: input.rightConnectionId || undefined };
  }
  return { path: input.leftPath, connectionId: input.leftConnectionId || undefined };
}
