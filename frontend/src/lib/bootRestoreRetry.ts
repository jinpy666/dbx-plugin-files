/**
 * Boot 恢复（页面刷新/宿主重启后宿主恢复的插件工作台 tab）首拉失败的有界
 * 重试决策。对标 ssh 插件 frontend/src/lib/connectRetry.ts 的 BOOT_RESTORE
 * 窗口位（dbx-plugin-ssh#144）：web/docker 部署下宿主对恢复 tab 的 connect
 * 重放（凭据重推）可能晚于恢复页首个 files/* 数秒落地，issue #68 的一次性
 * 自愈（reopen + 重试一次）在重放晚到时把恢复页留在空列表上。boot 恢复的
 * not-ready 失败因此获得一个有界固定节奏窗口吸收时序差，窗口耗尽才落
 * 持久横幅（保留手动重试出口）。
 */

export type BootRestoreRetryDecision = { kind: "retry"; attempt: number; delayMs: number } | { kind: "fail" };

/** 12 轮 × 1s ≈ 12s：介于 ssh preconnect 窗口（10s）与手动 inactive 窗口之间。 */
export const BOOT_RESTORE_RETRY_MAX = 12;
export const BOOT_RESTORE_RETRY_DELAY_MS = 1000;

/**
 * 决定 boot 恢复自愈的下一步。`attempt` 是已消耗的重试轮数（0 = 首轮）。
 * 决策保持纯函数，窗口边界与节奏可单测（ssh 同款收口）。
 */
export function decideBootRestoreRetry(options: { attempt: number }): BootRestoreRetryDecision {
  if (options.attempt >= BOOT_RESTORE_RETRY_MAX) return { kind: "fail" };
  return { kind: "retry", attempt: options.attempt + 1, delayMs: BOOT_RESTORE_RETRY_DELAY_MS };
}
