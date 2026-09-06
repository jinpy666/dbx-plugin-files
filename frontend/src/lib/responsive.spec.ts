import { describe, expect, it } from "vitest";
import { NARROW_VIEWPORT_MAX, isNarrowViewport } from "./responsive";

describe("isNarrowViewport", () => {
  it("treats <900px as narrow (P1-2 720px case)", () => {
    expect(isNarrowViewport(720)).toBe(true);
    expect(isNarrowViewport(899)).toBe(true);
  });

  it("keeps >=900px as normal", () => {
    expect(isNarrowViewport(NARROW_VIEWPORT_MAX)).toBe(false);
    expect(isNarrowViewport(1280)).toBe(false);
    expect(isNarrowViewport(0)).toBe(true);
  });
});
