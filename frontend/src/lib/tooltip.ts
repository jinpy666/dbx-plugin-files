// v-tip：宿主 webview（如 macOS WKWebView）不渲染原生 title 提示，
// 图标按钮统一改用本指令绘制的全局悬浮 tooltip，并把文案同步写入 aria-label。
import type { Directive } from "vue";

const SHOW_DELAY_MS = 350;
const GAP = 6;
const EDGE = 4;

let tipEl: HTMLElement | null = null;
let activeTarget: HTMLElement | null = null;
let showTimer = 0;

function hideTip(): void {
  window.clearTimeout(showTimer);
  if (tipEl) tipEl.classList.remove("is-visible");
  activeTarget = null;
}

function ensureTipEl(): HTMLElement {
  if (tipEl) return tipEl;
  tipEl = document.createElement("div");
  tipEl.className = "wb-global-tip";
  tipEl.setAttribute("role", "tooltip");
  document.body.appendChild(tipEl);
  // 滚动/缩放后锚点位置失效：capture 监听覆盖内层滚动容器，直接隐藏。
  window.addEventListener("scroll", hideTip, true);
  window.addEventListener("resize", hideTip);
  return tipEl;
}

function showTip(el: HTMLElement, text: string): void {
  if (!text) return;
  const tip = ensureTipEl();
  tip.textContent = text;
  tip.classList.add("is-visible");
  const rect = el.getBoundingClientRect();
  const size = tip.getBoundingClientRect();
  let top = rect.bottom + GAP;
  if (top + size.height > window.innerHeight - EDGE) top = rect.top - size.height - GAP;
  let left = rect.left + rect.width / 2 - size.width / 2;
  left = Math.min(Math.max(EDGE, left), Math.max(EDGE, window.innerWidth - size.width - EDGE));
  tip.style.top = `${Math.max(EDGE, top)}px`;
  tip.style.left = `${left}px`;
}

type TipHost = HTMLElement & { __vTipLeave?: () => void };

function scheduleShow(el: TipHost, text: () => string): void {
  window.clearTimeout(showTimer);
  showTimer = window.setTimeout(() => {
    activeTarget = el;
    showTip(el, text());
  }, SHOW_DELAY_MS);
}

function matchesFocusVisible(el: HTMLElement): boolean {
  try {
    return el.matches(":focus-visible");
  } catch {
    return false;
  }
}

export const vTip: Directive<TipHost, string | undefined> = {
  mounted(el, binding) {
    const text = () => (binding.value ?? "").trim();
    const syncLabel = () => {
      const value = text();
      if (value) el.setAttribute("aria-label", value);
      else el.removeAttribute("aria-label");
    };
    syncLabel();
    const leave = () => {
      window.clearTimeout(showTimer);
      if (activeTarget === el) hideTip();
    };
    el.addEventListener("mouseenter", () => scheduleShow(el, text));
    // 键盘聚焦（Tab）同样提示；鼠标点击带来的 focus 不触发。
    el.addEventListener("focus", () => {
      if (matchesFocusVisible(el)) scheduleShow(el, text);
    });
    el.addEventListener("mouseleave", leave);
    // 点击后立即收起，避免遮住随之打开的菜单/面板。
    el.addEventListener("mousedown", leave);
    el.addEventListener("blur", leave);
    el.__vTipLeave = leave;
  },
  updated(el, binding) {
    const value = (binding.value ?? "").trim();
    if (value) el.setAttribute("aria-label", value);
    else el.removeAttribute("aria-label");
    // 文案随语言/状态变化时，正在展示的 tooltip 同步刷新。
    if (activeTarget === el && tipEl && value) tipEl.textContent = value;
  },
  unmounted(el) {
    el.__vTipLeave?.();
  },
};
