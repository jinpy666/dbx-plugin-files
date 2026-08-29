import { describe, expect, it } from "vitest";
import { collapseCrumbs, parseCrumbs } from "./breadcrumbs";

describe("breadcrumbs", () => {
  it("parses root only", () => {
    expect(parseCrumbs("/")).toEqual([{ name: "/", path: "/" }]);
  });

  it("parses nested paths with navigable cumulative paths", () => {
    expect(parseCrumbs("/a/b/c")).toEqual([
      { name: "/", path: "/" },
      { name: "a", path: "/a" },
      { name: "b", path: "/a/b" },
      { name: "c", path: "/a/b/c" },
    ]);
  });

  it("ignores trailing slashes and empty segments", () => {
    expect(parseCrumbs("/docs/")).toEqual([
      { name: "/", path: "/" },
      { name: "docs", path: "/docs" },
    ]);
    expect(parseCrumbs("//a//b")).toEqual([
      { name: "/", path: "/" },
      { name: "a", path: "/a" },
      { name: "b", path: "/a/b" },
    ]);
  });

  it("keeps short paths intact", () => {
    const crumbs = parseCrumbs("/a/b");
    expect(collapseCrumbs(crumbs, 4)).toEqual(crumbs);
    expect(collapseCrumbs(crumbs, 2)).toEqual(crumbs); // clamp 到最小 3 也不够折叠
  });

  it("collapses middle levels into a marker for deep paths", () => {
    const crumbs = parseCrumbs("/a/b/c/d/e");
    // maxVisible 计可见层级（不含省略号）：根 + 末尾 3 级
    expect(collapseCrumbs(crumbs, 4)).toEqual([
      { name: "/", path: "/" },
      { collapsed: true },
      { name: "c", path: "/a/b/c" },
      { name: "d", path: "/a/b/c/d" },
      { name: "e", path: "/a/b/c/d/e" },
    ]);
  });

  it("collapses to root + tail when maxVisible is 3", () => {
    const crumbs = parseCrumbs("/a/b/c/d");
    expect(collapseCrumbs(crumbs, 3)).toEqual([
      { name: "/", path: "/" },
      { collapsed: true },
      { name: "c", path: "/a/b/c" },
      { name: "d", path: "/a/b/c/d" },
    ]);
  });
});
