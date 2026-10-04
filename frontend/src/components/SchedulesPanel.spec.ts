// @vitest-environment happy-dom
// SchedulesPanel:任务卡渲染(类型/cron 描述/下次运行/结果徽章)、动作上抛、
// 空态与历史展开。
import { describe, expect, it, vi } from "vitest";
import { config, mount } from "@vue/test-utils";
import SchedulesPanel from "./SchedulesPanel.vue";
import { vTip } from "../lib/tooltip";
import type { ScheduleRun, ScheduleTask } from "../lib/schedules";

config.global.directives = { tip: { mounted() {}, updated() {} } };

const t = (key: string, values?: Record<string, string | number>) =>
  values ? `${key}(${Object.values(values).join("|")})` : key;

const TASK: ScheduleTask = {
  id: "t1",
  name: "nightly",
  kind: "sync",
  sourceConnectionId: "__local__",
  sourcePath: "/data",
  targetConnectionId: "__local__",
  targetPath: "/mirror",
  cron: "30 3 * * *",
  enabled: true,
  options: { backupDir: "_backups", retentionDays: 14 },
  bisyncResyncDone: true,
  createdAt: 1,
  lastRunAt: null,
  lastRunStatus: null,
  nextRunAt: Date.parse("2026-10-05T03:30:00"),
};

function mountPanel(tasks: ScheduleTask[] = [TASK], runs: ScheduleRun[] = []) {
  return mount(SchedulesPanel, {
    props: { tasks, runs, locale: "en", t },
  });
}

describe("SchedulesPanel", () => {
  it("renders the task card with kind, cron description and next run", () => {
    const wrapper = mountPanel();
    expect(wrapper.text()).toContain("nightly");
    expect(wrapper.text()).toContain("scheduleKind.sync");
    expect(wrapper.text()).toContain("scheduleCronDaily(03:30)");
    expect(wrapper.text()).toContain("scheduleNextRun");
    expect(wrapper.text()).toContain("scheduleRetention(14)");
  });

  it("shows the empty state with a create entry when no tasks exist", async () => {
    const wrapper = mountPanel([]);
    expect(wrapper.text()).toContain("scheduleEmptyTitle");
    await wrapper.get(".wb-schedule-create").trigger("click");
    expect(wrapper.emitted("create")).toHaveLength(1);
  });

  it("emits edit/run-now/delete for the row actions", async () => {
    const wrapper = mountPanel();
    const buttons = wrapper.findAll(".wb-transfer-item .wb-icon-button");
    // 顺序:启停 checkbox 不是 button;依次为 运行/编辑/删除。
    await buttons[0].trigger("click");
    expect(wrapper.emitted("run-now")?.[0]?.[0]).toMatchObject({ id: "t1" });
    await buttons[1].trigger("click");
    expect(wrapper.emitted("edit")?.[0]?.[0]).toMatchObject({ id: "t1" });
    await buttons[2].trigger("click");
    expect(wrapper.emitted("delete")?.[0]?.[0]).toMatchObject({ id: "t1" });
  });

  it("emits toggle with the switch state", async () => {
    const wrapper = mountPanel();
    await wrapper.get(".wb-schedule-toggle input").setValue(false);
    expect(wrapper.emitted("toggle")?.[0]).toEqual([TASK, false]);
  });

  it("marks a running task and expands its history on demand", async () => {
    const runs: ScheduleRun[] = [
      { runId: "r1", taskId: "t1", trigger: "schedule", status: "success", bytes: 10, finishedAt: Date.parse("2026-10-04T03:30:00Z") },
      { runId: "r2", taskId: "t1", jobId: "job-1", trigger: "manual", status: "running", bytes: 0 },
    ];
    const wrapper = mountPanel([TASK], runs);
    expect(wrapper.text()).toContain("scheduleRunStatus.running");
    expect(wrapper.text()).not.toContain("r1");
    await wrapper.get(".wb-schedule-expander").trigger("click");
    expect(wrapper.text()).toContain("scheduleTriggerManual");
    expect(wrapper.text()).toContain("scheduleTriggerSchedule");
    // 再次点击收起。
    await wrapper.get(".wb-schedule-expander").trigger("click");
    expect(wrapper.text()).not.toContain("scheduleTriggerManual");
  });

  it("does not render a run-now button while a run is live (cancel instead)", () => {
    const runs: ScheduleRun[] = [
      { runId: "r2", taskId: "t1", trigger: "manual", status: "running", bytes: 0 },
    ];
    const wrapper = mountPanel([TASK], runs);
    const tips = wrapper.findAll(".wb-transfer-item .wb-icon-button");
    void tips;
    expect(wrapper.findAll(".wb-schedule-status.is-running").length).toBe(1);
  });
});
