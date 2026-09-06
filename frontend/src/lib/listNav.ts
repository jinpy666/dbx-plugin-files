// 文件列表键盘导航状态机（纯函数，FileTable.vue 消费）。
// 方向键移动活动行、Shift 从锚点连续扩选、Home/End 跳首尾；滚动跟随换算
// 也在这里，便于脱离组件单测。

export interface ListNavState {
  /** 当前活动行下标；-1 表示尚无活动行。 */
  index: number;
  /** Shift 扩选锚点下标；-1 表示未建立锚点。 */
  anchor: number;
}

export type ListNavKey = "up" | "down" | "home" | "end";

function clampIndex(index: number, total: number): number {
  return Math.min(total - 1, Math.max(0, index));
}

/**
 * 从 `state` 按 `key` 计算下一个导航状态。`extend`（Shift）保持锚点，
 * 普通移动把锚点重置到新位置；空列表原样返回。无活动行时 up 落到末行、
 * down 落到首行，与主流文件管理器一致。
 */
export function listNav(state: ListNavState, key: ListNavKey, total: number, extend: boolean): ListNavState {
  if (total <= 0) return state;
  // 尚无活动行：down/Home 落首行，up/End 落末行，与主流文件管理器一致。
  if (state.index < 0) {
    const edge = key === "up" || key === "end" ? total - 1 : 0;
    return { index: edge, anchor: extend ? state.anchor : edge };
  }
  const from = clampIndex(state.index, total);
  let index = from;
  if (key === "up") index = Math.max(0, from - 1);
  else if (key === "down") index = Math.min(total - 1, from + 1);
  else if (key === "home") index = 0;
  else index = total - 1;
  const anchor = extend && state.anchor >= 0 ? state.anchor : index;
  return { index, anchor };
}

/** 锚点与活动行之间的连续选择区间（含两端）。 */
export function selectionRange(state: ListNavState): [number, number] {
  return [Math.min(state.index, state.anchor), Math.max(state.index, state.anchor)];
}

/**
 * 计算让 `index` 行完整可见所需的 scrollTop；已可见则维持原值。
 * 返回新值由调用方写回容器，避免直接操作 DOM。
 */
export function scrollRowIntoView(
  scroll: { scrollTop: number; clientHeight: number },
  index: number,
  rowHeight: number,
): number {
  const top = index * rowHeight;
  const bottom = top + rowHeight;
  if (top < scroll.scrollTop) return top;
  if (bottom > scroll.scrollTop + scroll.clientHeight) return bottom - scroll.clientHeight;
  return scroll.scrollTop;
}
