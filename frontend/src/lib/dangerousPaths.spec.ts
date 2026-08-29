import { describe, expect, it } from "vitest";
import { inspect, requiresConfirm } from "./dangerousPaths";

describe("dangerousPaths", () => {
  it("refuses purge on root outright", () => {
    const result = inspect("purge", { path: "/" });
    expect(result.level).toBe("danger");
    expect(result.hits.some((hit) => hit.id === "purge-root")).toBe(true);
    expect(requiresConfirm(result)).toBe(true);
  });

  it("refuses purge on the configured root", () => {
    const result = inspect("purge", { path: "/data/", root: "/data" });
    expect(result.level).toBe("danger");
    expect(result.hits.some((hit) => hit.id === "purge-root")).toBe(true);
  });

  it("marks system paths on purge", () => {
    const result = inspect("purge", { path: "/etc/nginx" });
    expect(result.level).toBe("danger");
    expect(result.hits.some((hit) => hit.id === "system-path")).toBe(true);
  });

  it("treats plain purge as danger without system flag", () => {
    const result = inspect("purge", { path: "/data/tmp" });
    expect(result.level).toBe("danger");
    expect(result.hits.some((hit) => hit.id === "system-path")).toBe(false);
  });

  it("escalates recursive delete to danger and bulk file delete to warn", () => {
    const dir = inspect("delete", { targets: [{ path: "/a/b", kind: "directory" }] });
    expect(dir.level).toBe("danger");
    const bulk = inspect("delete", { targets: Array.from({ length: 11 }, (_, index) => ({ path: `/f${index}`, kind: "file" as const })) });
    expect(bulk.level).toBe("warn");
    const small = inspect("delete", { targets: [{ path: "/a.txt", kind: "file" }] });
    expect(small.level).toBe("none");
    expect(requiresConfirm(small)).toBe(false);
  });

  it("always confirms syncDir overwrite and warns for copyDir", () => {
    expect(inspect("syncDir", { path: "/backup" }).level).toBe("danger");
    expect(inspect("copyDir", { path: "/backup" }).level).toBe("warn");
  });
});
