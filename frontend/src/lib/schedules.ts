// 计划任务仓：cron 备份任务的类型、API 与本地 cron 工具。
// 后端契约见 docs/PROTOCOL.zh-CN.md 的 files/schedule/* 一节；cron 语义
// （Vixie 规则：dom/dow 双受限取 OR）与 backend/src/scheduler/cron.rs 对齐，
// 这里的解析只做即时反馈，后端仍是唯一校验者。

import { call } from "./api";

export type ScheduleKind = "sync" | "copy" | "bisync";

export type RunStatus = "running" | "success" | "failed" | "canceled" | "skipped";

export interface ScheduleOptions {
  dryRun?: boolean;
  maxDelete?: number;
  include?: string[];
  exclude?: string[];
  backupDir?: string;
  suffix?: string;
  metadata?: boolean;
  update?: boolean;
  existing?: boolean;
  immutable?: boolean;
  minSize?: string;
  maxSize?: string;
  minAge?: string;
  maxAge?: string;
  transfers?: number;
  checkers?: number;
  retries?: number;
  verifyAfter?: boolean;
  retentionDays?: number;
}

export interface ScheduleTask {
  id: string;
  name: string;
  kind: ScheduleKind;
  sourceConnectionId: string;
  sourcePath: string;
  targetConnectionId: string;
  targetPath: string;
  cron: string;
  enabled: boolean;
  options: ScheduleOptions;
  bisyncResyncDone: boolean;
  createdAt: number;
  lastRunAt?: number | null;
  lastRunStatus?: RunStatus | null;
  nextRunAt?: number | null;
}

export interface ScheduleRun {
  runId: string;
  taskId: string;
  jobId?: string;
  trigger: "schedule" | "manual";
  status: RunStatus;
  startedAt?: number | null;
  finishedAt?: number | null;
  bytes: number;
  files?: number | null;
  error?: string | null;
}

// ---- API -------------------------------------------------------------------

export const schedulesApi = {
  list: () => call<{ tasks: ScheduleTask[] }>("files/schedule/list"),
  create: (task: Omit<ScheduleTask, "id" | "createdAt" | "bisyncResyncDone">) =>
    call<{ task: ScheduleTask }>("files/schedule/create", task as unknown as Record<string, unknown>),
  update: (task: ScheduleTask) =>
    call<{ task: ScheduleTask }>("files/schedule/update", task as unknown as Record<string, unknown>),
  remove: (id: string) => call<{ removed: boolean }>("files/schedule/delete", { id }),
  history: (id?: string, limit = 100) =>
    call<{ runs: ScheduleRun[] }>("files/schedule/history", { id, limit }),
  runNow: (id: string) => call<{ run: ScheduleRun }>("files/schedule/runNow", { id }),
  cancel: (id: string) => call<{ success: boolean }>("files/schedule/cancel", { id }),
};

// ---- cron 解析（与 scheduler/cron.rs 同语义的即时校验/预览） -----------------

interface CronFields {
  minutes: boolean[];
  hours: boolean[];
  daysOfMonth: boolean[];
  months: boolean[];
  daysOfWeek: boolean[];
  domAny: boolean;
  dowAny: boolean;
}

function parseCronField(field: string, min: number, max: number): boolean[] | null {
  const values = new Array<boolean>(max + 1).fill(false);
  if (!field.trim()) return null;
  for (const part of field.split(",")) {
    const piece = part.trim();
    if (!piece) return null;
    const [rangeText, stepText] = piece.split("/");
    let step = 1;
    if (stepText !== undefined) {
      step = Number(stepText);
      if (!Number.isInteger(step) || step < 1) return null;
    }
    let lo: number;
    let hi: number;
    if (rangeText === "*") {
      lo = min;
      hi = max;
    } else if (rangeText.includes("-")) {
      const [a, b] = rangeText.split("-");
      lo = Number(a);
      hi = Number(b);
    } else {
      lo = Number(rangeText);
      hi = step === 1 ? lo : max;
    }
    if (!Number.isInteger(lo) || !Number.isInteger(hi) || lo < min || hi > max || lo > hi) return null;
    for (let value = lo; value <= hi; value += step) values[value] = true;
  }
  return values;
}

/** 解析五字段 cron；非法返回 null（`7` 折叠为周日，Vixie 约定）。 */
export function parseCron(expr: string): CronFields | null {
  const fields = expr.trim().split(/\s+/);
  if (fields.length !== 5) return null;
  const minutes = parseCronField(fields[0], 0, 59);
  const hours = parseCronField(fields[1], 0, 23);
  const daysOfMonth = parseCronField(fields[2], 1, 31);
  const months = parseCronField(fields[3], 1, 12);
  const daysOfWeek = parseCronField(fields[4], 0, 7);
  if (!minutes || !hours || !daysOfMonth || !months || !daysOfWeek) return null;
  if (daysOfWeek[7]) {
    daysOfWeek[7] = false;
    daysOfWeek[0] = true;
  }
  return {
    minutes,
    hours,
    daysOfMonth,
    months,
    daysOfWeek,
    domAny: fields[2].trim() === "*",
    dowAny: fields[4].trim() === "*",
  };
}

export function isValidCron(expr: string): boolean {
  return parseCron(expr) !== null;
}

function dayMatches(fields: CronFields, date: Date): boolean {
  const dom = fields.daysOfMonth[date.getDate()] === true;
  const dow = fields.daysOfWeek[date.getDay()] === true;
  if (fields.domAny && fields.dowAny) return true;
  if (fields.domAny) return dow;
  if (fields.dowAny) return dom;
  return dom || dow;
}

/** `from` 之后的下一次触发时刻（分钟精度，本地时区）；一年内无触发返回 null。 */
export function nextRunAfter(expr: string, from = new Date()): Date | null {
  const fields = parseCron(expr);
  if (!fields) return null;
  const cursor = new Date(from.getTime());
  cursor.setSeconds(0, 0);
  cursor.setMinutes(cursor.getMinutes() + 1);
  for (let step = 0; step < 400; step += 1) {
    if (!fields.months[cursor.getMonth() + 1]) {
      cursor.setDate(1);
      cursor.setHours(0, 0, 0, 0);
      cursor.setMonth(cursor.getMonth() + 1);
      continue;
    }
    if (!dayMatches(fields, cursor)) {
      cursor.setDate(cursor.getDate() + 1);
      cursor.setHours(0, 0, 0, 0);
      continue;
    }
    // 分钟扫描严格限制在当日（从当日零点起算的剩余分钟数）：跨过午夜就会
    // 把次日的 00:00 误判为本日命中（Rust 版 scan_day 用 day_end 兜底，同款
    // 边界在这里用剩余分钟数表达）。
    const probe = new Date(cursor.getTime());
    const dayStart = new Date(cursor.getTime());
    dayStart.setHours(0, 0, 0, 0);
    const minuteOfProbe = Math.round((probe.getTime() - dayStart.getTime()) / 60000);
    for (let minute = minuteOfProbe; minute < 24 * 60; minute += 1) {
      if (fields.hours[probe.getHours()] && fields.minutes[probe.getMinutes()]) return probe;
      probe.setMinutes(probe.getMinutes() + 1);
    }
    cursor.setDate(cursor.getDate() + 1);
    cursor.setHours(0, 0, 0, 0);
  }
  return null;
}

// ---- cron 人类可读描述（预设识别 + Intl 兜底） ------------------------------

const twoDigits = (value: number): string => String(value).padStart(2, "0");

function fieldIs(expr: string, index: number, value: string): boolean {
  return expr.trim().split(/\s+/)[index] === value;
}

/**
 * 识别常见预设并生成本地化描述；无法识别时返回「自定义」+ 原文。
 * `t` 由调用方注入（七语字典），星期/时间用 Intl 按宿主 locale 呈现。
 */
export function describeCron(expr: string, locale: string, t: (key: string, values?: Record<string, string | number>) => string): string {
  const fields = expr.trim().split(/\s+/);
  if (fields.length !== 5) return expr;
  const [minute, hour, dom, month, dow] = fields;
  if (month !== "*") return t("scheduleCronCustom", { expr });
  const time = `${twoDigits(Number(hour))}:${twoDigits(Number(minute))}`;
  // 每小时：m * * * *
  if (hour === "*" && dom === "*" && dow === "*" && /^\d+$/.test(minute)) {
    return t("scheduleCronHourly", { minute: twoDigits(Number(minute)) });
  }
  if (!/^\d+$/.test(minute) || !/^\d+$/.test(hour)) return t("scheduleCronCustom", { expr });
  // 每天：m h * * *
  if (dom === "*" && dow === "*") {
    return t("scheduleCronDaily", { time });
  }
  // 每周：m h * * d（单值；7 已视作 0）
  if (dom === "*" && /^\d+$/.test(dow)) {
    const weekday = new Intl.DateTimeFormat(locale, { weekday: "long" }).format(new Date(2024, 0, 7 + (Number(dow) % 7)));
    return t("scheduleCronWeekly", { weekday, time });
  }
  // 每月：m h d * *
  if (dow === "*" && /^\d+$/.test(dom)) {
    return t("scheduleCronMonthly", { day: Number(dom), time });
  }
  return t("scheduleCronCustom", { expr });
}

// ---- 预设（对话框快捷选择） --------------------------------------------------

export const CRON_PRESETS = [
  { id: "hourly", cron: "0 * * * *" },
  { id: "daily3", cron: "30 3 * * *" },
  { id: "daily12", cron: "0 12 * * *" },
  { id: "weekly", cron: "30 3 * * 0" },
] as const;

export function presetIdOf(cron: string): string {
  const preset = CRON_PRESETS.find((entry) => entry.cron === cron);
  return preset ? preset.id : "custom";
}

/** 任务当前是否有一笔进行中的运行（运行历史驱动，不依赖传输前缀）。 */
export function isTaskRunning(runs: readonly ScheduleRun[], taskId: string): boolean {
  return runs.some((run) => run.taskId === taskId && run.status === "running");
}

/** 面板结果徽章用的运行状态归一：任务行取最近一笔终态。 */
export function lastFinishedRun(runs: readonly ScheduleRun[], taskId: string): ScheduleRun | undefined {
  return runs.find((run) => run.taskId === taskId && run.status !== "running");
}
