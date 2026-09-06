import { describe, expect, it } from "vitest";
import { listNav, scrollRowIntoView, selectionRange, type ListNavState } from "./listNav";

const at = (index: number, anchor = index): ListNavState => ({ index, anchor });

describe("listNav", () => {
  it("moves up/down one row and resets the anchor without Shift", () => {
    expect(listNav(at(2), "down", 5, false)).toEqual(at(3));
    expect(listNav(at(2), "up", 5, false)).toEqual(at(1));
  });

  it("clamps at both ends", () => {
    expect(listNav(at(0), "up", 5, false)).toEqual(at(0));
    expect(listNav(at(4), "down", 5, false)).toEqual(at(4));
  });

  it("starts from the edges when no row is active yet", () => {
    expect(listNav({ index: -1, anchor: -1 }, "down", 5, false)).toEqual(at(0));
    expect(listNav({ index: -1, anchor: -1 }, "up", 5, false)).toEqual(at(4));
  });

  it("keeps the anchor while extending with Shift", () => {
    expect(listNav(at(1, 3), "down", 5, true)).toEqual({ index: 2, anchor: 3 });
    expect(listNav(at(1, 3), "up", 5, true)).toEqual({ index: 0, anchor: 3 });
  });

  it("jumps to first/last with Home/End", () => {
    expect(listNav(at(3), "home", 5, false)).toEqual(at(0));
    expect(listNav(at(3), "end", 5, false)).toEqual(at(4));
  });

  it("is a no-op on an empty list", () => {
    expect(listNav(at(2), "down", 0, false)).toEqual(at(2));
  });

  it("returns the inclusive selection range regardless of direction", () => {
    expect(selectionRange(at(4, 2))).toEqual([2, 4]);
    expect(selectionRange(at(1, 3))).toEqual([1, 3]);
  });
});

describe("scrollRowIntoView", () => {
  it("keeps the offset when the row is already visible", () => {
    expect(scrollRowIntoView({ scrollTop: 100, clientHeight: 280 }, 5, 28)).toBe(100);
  });

  it("jumps up when the row is above the viewport", () => {
    expect(scrollRowIntoView({ scrollTop: 280, clientHeight: 280 }, 5, 28)).toBe(140);
  });

  it("scrolls down so the row bottom aligns with the viewport", () => {
    expect(scrollRowIntoView({ scrollTop: 0, clientHeight: 280 }, 20, 28)).toBe(308);
  });
});
