// 窄视口判定（P1-2）：<900px 属「半屏/分屏窄窗口」，App 在跨入该区间时
// 默认一次性收起 dock 与双栏，避免左栏主区被静默挤压（overflow:hidden 裁切）。
export const NARROW_VIEWPORT_MAX = 900;

export function isNarrowViewport(width: number): boolean {
  return width < NARROW_VIEWPORT_MAX;
}
