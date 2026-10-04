import { describe, expect, it } from "vitest";
import {
  BOOT_RESTORE_FAST_FAIL_MS,
  BOOT_RESTORE_RETRY_DELAY_MS,
  BOOT_RESTORE_RETRY_MAX,
  decideBootRestoreRetry,
} from "./bootRestoreRetry";

describe("decideBootRestoreRetry", () => {
  it("polls at the fixed restore cadence inside the bounded window", () => {
    expect(decideBootRestoreRetry({ attempt: 0 })).toEqual({ kind: "retry", attempt: 1, delayMs: BOOT_RESTORE_RETRY_DELAY_MS });
    expect(decideBootRestoreRetry({ attempt: 5 })).toEqual({ kind: "retry", attempt: 6, delayMs: BOOT_RESTORE_RETRY_DELAY_MS });
    const last = decideBootRestoreRetry({ attempt: BOOT_RESTORE_RETRY_MAX - 1 });
    expect(last).toEqual({ kind: "retry", attempt: BOOT_RESTORE_RETRY_MAX, delayMs: BOOT_RESTORE_RETRY_DELAY_MS });
  });

  it("fails once the restore window is exhausted", () => {
    // dbx-plugin-files#144 同源：宿主 connect 重放始终未落地时不能无限轮询，
    // 窗口耗尽必须落持久横幅（保留手动重试出口）。
    expect(decideBootRestoreRetry({ attempt: BOOT_RESTORE_RETRY_MAX })).toEqual({ kind: "fail" });
    expect(decideBootRestoreRetry({ attempt: BOOT_RESTORE_RETRY_MAX + 3 })).toEqual({ kind: "fail" });
  });

  it("fails fast when the failed reload was a real timeout, not a boot race", () => {
    // E2E 实证：存储停机时 files/list 挂满 30s 操作超时，续窗口只会把恢复页
    // 按在分钟级骨架屏上——单轮真超时直接落持久横幅（ssh 同语义）。
    expect(decideBootRestoreRetry({ attempt: 0, attemptMs: BOOT_RESTORE_FAST_FAIL_MS })).toEqual({ kind: "fail" });
    expect(decideBootRestoreRetry({ attempt: 0, attemptMs: 30_000 })).toEqual({ kind: "fail" });
    expect(decideBootRestoreRetry({ attempt: 3, attemptMs: BOOT_RESTORE_FAST_FAIL_MS - 1 })).toEqual({
      kind: "retry",
      attempt: 4,
      delayMs: BOOT_RESTORE_RETRY_DELAY_MS,
    });
  });
});
