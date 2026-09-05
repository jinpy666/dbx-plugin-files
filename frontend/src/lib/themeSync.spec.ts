// 薄 spec：验证 shared/frontend/themeSync 在本插件工具链下 import 解析与行为成立。
import { describe, expect, it } from "vitest";
import { themeBridgeCss, THEME_BRIDGE_STYLE_ID } from "../../../../shared/frontend/themeSync";

describe("themeBridgeCss", () => {
  it("bridges plugin vars onto host theme tokens", () => {
    const css = themeBridgeCss();
    expect(css).toContain("--background:var(--color-background,#131416)");
    expect(css).toContain("--primary:var(--color-primary,#3b82f6)");
    expect(css).toContain("--popover:var(--color-popover,");
    expect(css).toContain("--terminal-font-family:var(--font-mono,");
    expect(css).toContain("--mono-font-family:var(--font-mono,");
    expect(css).toContain(':root[data-dbx-theme="light"]{color-scheme:light}');
  });

  it("allows per-plugin fallback overrides", () => {
    const css = themeBridgeCss({ "--background": "#000000" });
    expect(css).toContain("--background:var(--color-background,#000000)");
    expect(css).toContain("--foreground:var(--color-foreground,#d7d7db)");
  });

  it("exposes the stable style element id", () => {
    expect(THEME_BRIDGE_STYLE_ID).toBe("dbx-host-theme-bridge");
  });
});
