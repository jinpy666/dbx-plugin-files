import { describe, expect, it } from "vitest";
import { resolveUploadTarget } from "./uploadTarget";

describe("resolveUploadTarget", () => {
  it("routes uploads to the remote target pane in dual-pane mode (P1-5)", () => {
    // 双栏默认：左栏本地 __local__，右栏同连接（undefined=当前连接）
    expect(
      resolveUploadTarget({
        dualPane: true,
        leftPath: "/Users/demo",
        rightPath: "/docs",
        leftConnectionId: "__local__",
      }),
    ).toEqual({ path: "/docs" });
  });

  it("keeps the right pane explicit connection in dual-pane mode", () => {
    expect(
      resolveUploadTarget({
        dualPane: true,
        leftPath: "/Users/demo",
        rightPath: "/data",
        leftConnectionId: "__local__",
        rightConnectionId: "conn-b",
      }),
    ).toEqual({ path: "/data", connectionId: "conn-b" });
  });

  it("keeps single-pane uploads on the current pane (current connection)", () => {
    expect(
      resolveUploadTarget({
        dualPane: false,
        leftPath: "/docs",
        rightPath: "/ignored",
        leftConnectionId: undefined,
      }),
    ).toEqual({ path: "/docs" });
  });

  it("never uploads into the local pane even if it is the left pane", () => {
    // 左栏切到本地时，双栏上传目标仍是右栏（远端），绝不落 __local__。
    const target = resolveUploadTarget({
      dualPane: true,
      leftPath: "/Users/demo",
      rightPath: "/",
      leftConnectionId: "__local__",
    });
    expect(target.path).toBe("/");
    expect(target.connectionId).toBeUndefined();
    expect(target.connectionId).not.toBe("__local__");
  });
});
