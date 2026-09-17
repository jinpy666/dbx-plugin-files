// 前端 a11y 键盘交互共享层（审计中等组 #7/#8/#12）：
// ① trapTabKey——对话框焦点陷阱（与 ConfirmDialog 既有实现同语义）；
// ② onTablistArrowKeys——role=tablist 的 ←→ 循环切换（WAI-ARIA tabs pattern）；
// ③ onMenuArrowKeys——role=menu 的 ↑↓/Home/End 在 menuitem 间移动焦点。

/** 可聚焦控件选择器：disabled 排除、负 tabindex（显式移出 Tab 序）排除。 */
export const FOCUSABLE_SELECTOR = 'button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])';

/** role=menu 下可键盘触发的菜单项（禁用项跳过）。 */
export const MENU_ITEM_SELECTOR = '[role="menuitem"]:not([disabled])';

/**
 * Tab 焦点陷阱（对话框用，参照 ConfirmDialog.vue 既有实现）：
 * 焦点已末位时 Tab 回绕到首位，Shift+Tab 反向；焦点意外落在容器外（BODY）
 * 时也拉回容器内。defaultPrevented（如 CodeMirror 已消费 Tab 缩进）时跳过，
 * 不与编辑器键位冲突。
 */
export function trapTabKey(event: KeyboardEvent, root: HTMLElement | undefined | null) {
  if (event.key !== "Tab" || event.defaultPrevented) return;
  if (!root) return;
  const focusables = [...root.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)];
  if (!focusables.length) return;
  const first = focusables[0]!;
  const last = focusables[focusables.length - 1]!;
  const current = document.activeElement;
  const inside = current instanceof HTMLElement && root.contains(current);
  if (event.shiftKey) {
    if (!inside || current === first) {
      event.preventDefault();
      last.focus();
    }
    return;
  }
  if (!inside || current === last) {
    event.preventDefault();
    first.focus();
  }
}

/**
 * tablist 方向键导航：←→ 在同一 tablist 的 tab 间循环移动焦点并触发
 * click（复用组件既有切换逻辑与其错误路径，如 JSON 非法时禁止切 Form）。
 * 绑定在 role=tablist 容器上即可，按 event.target 定位当前 tab。
 */
export function onTablistArrowKeys(event: KeyboardEvent) {
  if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
  const target = event.target;
  if (!(target instanceof Element)) return;
  const tab = target.closest('[role="tab"]');
  const tablist = tab?.parentElement;
  if (!(tab instanceof HTMLElement) || !tablist) return;
  const tabs = [...tablist.querySelectorAll<HTMLElement>('[role="tab"]')];
  const index = tabs.indexOf(tab);
  if (index < 0) return;
  event.preventDefault();
  const next = tabs[event.key === "ArrowRight" ? (index + 1) % tabs.length : (index - 1 + tabs.length) % tabs.length];
  next?.focus();
  next?.click();
}

/**
 * 菜单（role=menu）键盘导航：↑↓ 循环移动、Home/End 跳首尾；Enter/Space
 * 由 menuitem 按钮原生触发，无需在此处理。焦点不在菜单项上时 ArrowDown
 * 落首项、ArrowUp 落末项。绑定在 role=menu 容器上。
 */
export function onMenuArrowKeys(event: KeyboardEvent) {
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp" && event.key !== "Home" && event.key !== "End") return;
  const root = event.currentTarget;
  if (!(root instanceof HTMLElement)) return;
  const items = [...root.querySelectorAll<HTMLElement>(MENU_ITEM_SELECTOR)];
  if (!items.length) return;
  const active = document.activeElement;
  const index = active instanceof HTMLElement ? items.indexOf(active) : -1;
  event.preventDefault();
  if (event.key === "Home") {
    items[0]?.focus();
    return;
  }
  if (event.key === "End") {
    items[items.length - 1]?.focus();
    return;
  }
  if (event.key === "ArrowDown") {
    items[(index + 1) % items.length]?.focus();
    return;
  }
  items[index <= 0 ? items.length - 1 : index - 1]?.focus();
}
