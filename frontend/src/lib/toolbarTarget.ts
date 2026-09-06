// 工具栏动作目标解析（R3-P1-2）：工具栏此前只绑定左栏——右栏选中时
// 「下载/删除所选」仍禁用、「新建文件夹」永远落左栏。收口为「最近活动栏」
// 路由（FileZilla 惯例）：最近点击/键盘操作的一栏承接新建/下载/删除等动作。
// 纯函数参照 uploadTarget.ts 模式；上传按钮不受 activeSide 影响
// （P1-5 已定「上传=传向远端」语义，仍走 resolveUploadTarget）。

export type PaneSide = "left" | "right";

export interface ToolbarTarget {
  /** 工具栏动作作用的栏（最近活动栏；单栏恒左栏）。 */
  side: PaneSide;
  /** 该栏当前选择集快照。 */
  paths: string[];
  /** 两栏取并集后的「有所选」判定（工具栏下载/删除按钮可用性）。 */
  hasSelection: boolean;
}

export function resolveToolbarTarget(input: {
  dualPane: boolean;
  activeSide: PaneSide;
  leftSelection: readonly string[];
  rightSelection: readonly string[];
}): ToolbarTarget {
  const side: PaneSide = input.dualPane && input.activeSide === "right" ? "right" : "left";
  const paths = side === "right" ? input.rightSelection : input.leftSelection;
  return {
    side,
    paths: [...paths],
    // 「有所选」取两栏并集（任一栏选中即启用下载/删除）；单栏模式只看左栏
    // （右栏不存在，避免收起双栏后的幽灵选中集误启用按钮）。
    hasSelection: input.dualPane
      ? input.leftSelection.length > 0 || input.rightSelection.length > 0
      : input.leftSelection.length > 0,
  };
}
