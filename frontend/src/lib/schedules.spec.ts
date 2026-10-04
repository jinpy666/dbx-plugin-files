// schedules.ts 纯函数:cron 解析/校验、下次运行预览(与后端 scheduler/cron.rs
// 同语义)、预设识别与人类可读描述。
import { describe, expect, it } from "vitest";
import {
  CRON_PRESETS,
  describeCron,
  isTaskRunning,
  isValidCron,
  lastFinishedRun,
  nextRunAfter,
  parseCron,
  presetIdOf,
  type ScheduleRun,
} from "./schedules";

const t = (key: string, values?: Record<string, string | number>) =>
  values ? `${key}(${Object.values(values).join("|")})` : key;

describe("parseCron/isValidCron", () => {
  it("accepts the documented shapes and folds dow 7 onto Sunday", () => {
    expect(isValidCron("* * * * *")).toBe(true);
    expect(isValidCron("*/15 * * * *")).toBe(true);
    expect(isValidCron("5,35,50-59/5 * * * *")).toBe(true);
    expect(isValidCron("30 3 * * 7")).toBe(true);
    expect(parseCron("30 3 * * 7")).toEqual(parseCron("30 3 * * 0"));
  });

  it("rejects malformed expressions", () => {
    for (const bad of ["", "* * * *", "* * * * * *", "60 * * * *", "*/0 * * * *", "1-0 * * * *", "a * * * *"]) {
      expect(isValidCron(bad), bad).toBe(false);
    }
  });
});

describe("nextRunAfter", () => {
  // 固定基准:2026-10-04 是周日(本地时区;断言只比较本地墙钟字段)。
  const from = new Date(2026, 9, 4, 10, 7, 30);

  it("fires strictly after, minute resolution", () => {
    expect(nextRunAfter("* * * * *", from)).toEqual(new Date(2026, 9, 4, 10, 8, 0));
  });

  it("snaps to the grid and rolls over the hour/day", () => {
    expect(nextRunAfter("*/15 * * * *", from)).toEqual(new Date(2026, 9, 4, 10, 15, 0));
    expect(nextRunAfter("30 3 * * *", new Date(2026, 9, 4, 3, 30, 0))).toEqual(new Date(2026, 9, 5, 3, 30, 0));
  });

  it("matches either dom or dow when both are restricted (Vixie rule)", () => {
    // 2026-10-04 周日之后:周五 10-09 先于周二 10-13。
    expect(nextRunAfter("0 0 13 * 5", from)).toEqual(new Date(2026, 9, 9, 0, 0, 0));
    // 从 10-09 00:00 之后:周二 10-13 先于下一个周五 10-16。
    expect(nextRunAfter("0 0 13 * 5", new Date(2026, 9, 9, 0, 30, 0))).toEqual(new Date(2026, 9, 13, 0, 0, 0));
  });

  it("returns null for impossible dates within the cap", () => {
    expect(nextRunAfter("0 0 31 2 *", from)).toBeNull();
  });

  it("rejects invalid expressions", () => {
    expect(nextRunAfter("not a cron", from)).toBeNull();
  });
});

describe("describeCron", () => {
  it("recognizes the presets and falls back to custom", () => {
    expect(describeCron("0 * * * *", "en", t)).toBe("scheduleCronHourly(00)");
    expect(describeCron("30 3 * * *", "en", t)).toBe("scheduleCronDaily(03:30)");
    expect(describeCron("30 3 * * 0", "en", t)).toContain("scheduleCronWeekly(Sunday|03:30)");
    expect(describeCron("0 12 1 * *", "en", t)).toBe("scheduleCronMonthly(1|12:00)");
    expect(describeCron("5,35 * * * *", "en", t)).toBe("scheduleCronCustom(5,35 * * * *)");
  });
});

describe("presetIdOf", () => {
  it("maps known crons to presets and everything else to custom", () => {
    expect(CRON_PRESETS.length).toBeGreaterThan(0);
    expect(presetIdOf(CRON_PRESETS[1].cron)).toBe(CRON_PRESETS[1].id);
    expect(presetIdOf("7 7 7 7 7")).toBe("custom");
  });
});

describe("run helpers", () => {
  const runs: ScheduleRun[] = [
    { runId: "r2", taskId: "t1", trigger: "manual", status: "failed", bytes: 0, error: "boom" },
    { runId: "r1", taskId: "t1", trigger: "schedule", status: "success", bytes: 5 },
    { runId: "r3", taskId: "t2", trigger: "schedule", status: "running", bytes: 0 },
  ];

  it("isTaskRunning keys on live runs", () => {
    expect(isTaskRunning(runs, "t2")).toBe(true);
    expect(isTaskRunning(runs, "t1")).toBe(false);
  });

  it("lastFinishedRun picks the newest terminal record", () => {
    expect(lastFinishedRun(runs, "t1")?.runId).toBe("r2");
    expect(lastFinishedRun(runs, "t2")).toBeUndefined();
  });
});
