// DBX 宿主外观契约的插件侧解析（与 ssh 插件 lib/appearance 同方案，去终端
// ANSI 调色板）：颜色令牌取自 DBX globals.css 的 `:root`（浅色 pearl，白底）
// 与 `.dark` 两个规范块。宿主只下发 colorScheme 或部分颜色时，缺失字段按当前
// 方案回退到规范值，避免出现拼色。

export const DBX_APPEARANCE_PALETTES: Record<"light" | "dark", DbxPluginAppearance["colors"]> = {
  light: {
    background: "rgb(255 255 255)",
    foreground: "rgb(10 10 10)",
    muted: "rgb(245 245 245)",
    mutedForeground: "rgb(115 115 115)",
    accent: "rgb(245 245 245)",
    accentForeground: "rgb(23 23 23)",
    border: "rgb(229 229 229)",
    destructive: "rgb(231 0 11)",
  },
  dark: {
    background: "rgb(19 20 22)",
    foreground: "rgb(215 215 219)",
    muted: "rgb(42 42 45)",
    mutedForeground: "rgb(151 152 157)",
    accent: "rgb(46 47 51)",
    accentForeground: "rgb(221 221 226)",
    border: "rgb(110 110 114 / 0.28)",
    destructive: "rgb(243 98 95)",
  },
};

// 弹层背景按 DBX `--popover` 规范值，白底不透光，深色比背景略亮。
export const DBX_POPOVER: Record<"light" | "dark", string> = {
  light: "rgb(255 255 255)",
  dark: "rgb(30 30 32)",
};

const DEFAULT_TERMINAL_FONT_FAMILY = "'Cascadia Mono', Consolas, monospace";
const DEFAULT_UI_FONT_FAMILY = "Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif";
const DEFAULT_TERMINAL_FONT_SIZE = 13;

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function isPositiveFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

// 宿主下发的 appearance 允许逐字段缺失（1.0 部分下发、1.1 theme 通道只带颜色令牌）。
export interface DbxPluginAppearanceInput {
  colorScheme?: "light" | "dark";
  colors?: Partial<DbxPluginAppearance["colors"]>;
  terminal?: Partial<DbxPluginAppearance["terminal"]>;
  ui?: Partial<NonNullable<DbxPluginAppearance["ui"]>>;
}

// 返回值保证 terminal/ui 字段存在，调用方无需再判空。
export function resolveAppearance(next?: DbxPluginAppearanceInput | null): DbxPluginAppearance & { ui: { fontFamily: string } } {
  const colorScheme = next?.colorScheme === "light" ? "light" : "dark";
  const colors: DbxPluginAppearance["colors"] = { ...DBX_APPEARANCE_PALETTES[colorScheme] };
  const hostColors = next?.colors as Record<string, unknown> | undefined;
  if (hostColors) {
    for (const key of Object.keys(colors) as Array<keyof DbxPluginAppearance["colors"]>) {
      if (isNonEmptyString(hostColors[key])) colors[key] = hostColors[key] as string;
    }
  }
  const fontFamily = isNonEmptyString(next?.terminal?.fontFamily) ? next!.terminal!.fontFamily : DEFAULT_TERMINAL_FONT_FAMILY;
  const fontSize = isPositiveFiniteNumber(next?.terminal?.fontSize) ? next!.terminal!.fontSize : DEFAULT_TERMINAL_FONT_SIZE;
  const uiFontFamily = isNonEmptyString(next?.ui?.fontFamily) ? next!.ui!.fontFamily : DEFAULT_UI_FONT_FAMILY;
  return {
    colorScheme,
    colors,
    terminal: { fontFamily, fontSize },
    ui: { fontFamily: uiFontFamily },
  };
}
