// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { createNavGuard } from "./navGuard";

describe("navGuard", () => {
  it("issues monotonically increasing tokens", () => {
    const guard = createNavGuard();
    expect(guard.next()).toBe(1);
    expect(guard.next()).toBe(2);
    expect(guard.next()).toBe(3);
  });

  it("accepts the latest token and rejects stale tokens (R3-P1-1)", () => {
    const guard = createNavGuard();
    const slow = guard.next(); // 先点慢 /docs
    const fast = guard.next(); // 再点快 /10k
    expect(guard.isCurrent(fast)).toBe(true);
    expect(guard.isCurrent(slow)).toBe(false);
  });

  it("keeps a token current until a newer navigation starts", () => {
    const guard = createNavGuard();
    const token = guard.next();
    expect(guard.isCurrent(token)).toBe(true);
    // 同一次导航的多次响应回调（进度/错误）仍有效
    expect(guard.isCurrent(token)).toBe(true);
    guard.next();
    expect(guard.isCurrent(token)).toBe(false);
  });

  it("guards are independent per pane", () => {
    const left = createNavGuard();
    const right = createNavGuard();
    const leftToken = left.next();
    right.next();
    // 右栏的新导航不应作废左栏的在途请求
    expect(left.isCurrent(leftToken)).toBe(true);
  });
});
