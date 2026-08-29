import { describe, expect, it } from "vitest";
import { isLargeDirectory, LARGE_DIRECTORY_THRESHOLD } from "./largeDir";

describe("large directory threshold", () => {
  it("flags directories at or above the threshold only", () => {
    expect(isLargeDirectory(LARGE_DIRECTORY_THRESHOLD)).toBe(true);
    expect(isLargeDirectory(10_000)).toBe(true);
    expect(isLargeDirectory(LARGE_DIRECTORY_THRESHOLD - 1)).toBe(false);
    expect(isLargeDirectory(0)).toBe(false);
  });

  it("never flags NaN or infinite counts", () => {
    expect(isLargeDirectory(Number.NaN)).toBe(false);
    expect(isLargeDirectory(Number.POSITIVE_INFINITY)).toBe(false);
  });

  it("honours a custom threshold (right pane shares the same rule)", () => {
    expect(isLargeDirectory(5, 5)).toBe(true);
    expect(isLargeDirectory(4, 5)).toBe(false);
  });
});
