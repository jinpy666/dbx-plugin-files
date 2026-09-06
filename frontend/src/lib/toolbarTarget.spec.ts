// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { resolveToolbarTarget } from "./toolbarTarget";

describe("resolveToolbarTarget (R3-P1-2)", () => {
  it("routes to the active pane in dual-pane mode", () => {
    const target = resolveToolbarTarget({
      dualPane: true,
      activeSide: "right",
      leftSelection: [],
      rightSelection: ["/docs/a.txt"],
    });
    expect(target.side).toBe("right");
    expect(target.paths).toEqual(["/docs/a.txt"]);
    expect(target.hasSelection).toBe(true);
  });

  it("defaults to the left pane when it is active", () => {
    const target = resolveToolbarTarget({
      dualPane: true,
      activeSide: "left",
      leftSelection: ["/a"],
      rightSelection: ["/b"],
    });
    expect(target.side).toBe("left");
    expect(target.paths).toEqual(["/a"]);
  });

  it("keeps hasSelection true from either pane (toolbar buttons enable off the union)", () => {
    const onlyRight = resolveToolbarTarget({
      dualPane: true,
      activeSide: "right",
      leftSelection: [],
      rightSelection: ["/b"],
    });
    expect(onlyRight.hasSelection).toBe(true);
    const onlyLeft = resolveToolbarTarget({
      dualPane: true,
      activeSide: "left",
      leftSelection: ["/a"],
      rightSelection: [],
    });
    expect(onlyLeft.hasSelection).toBe(true);
  });

  it("always resolves the left pane in single-pane mode regardless of activeSide", () => {
    const target = resolveToolbarTarget({
      dualPane: false,
      activeSide: "right",
      leftSelection: ["/a"],
      rightSelection: [],
    });
    expect(target.side).toBe("left");
    expect(target.paths).toEqual(["/a"]);
  });

  it("reports no selection when both panes are empty", () => {
    const target = resolveToolbarTarget({ dualPane: true, activeSide: "left", leftSelection: [], rightSelection: [] });
    expect(target.hasSelection).toBe(false);
    expect(target.paths).toEqual([]);
  });

  it("returns a selection snapshot that cannot be mutated from outside", () => {
    const rightSelection = ["/b"];
    const target = resolveToolbarTarget({ dualPane: true, activeSide: "right", leftSelection: [], rightSelection });
    target.paths.push("/c");
    expect(rightSelection).toEqual(["/b"]);
  });
});
