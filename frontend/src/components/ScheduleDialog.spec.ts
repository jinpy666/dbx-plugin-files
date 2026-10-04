// @vitest-environment happy-dom
// ScheduleDialog:新建默认值、cron 校验阻止提交、确认载荷组装(选项映射)、
// 编辑回填、目标连接切换跟随。
import { describe, expect, it } from "vitest";
import { config, mount } from "@vue/test-utils";
import ScheduleDialog from "./ScheduleDialog.vue";
import { vTip } from "../lib/tooltip";
import type { ScheduleTask } from "../lib/schedules";

config.global.directives = { tip: { mounted() {}, updated() {} } };

const t = (key: string, values?: Record<string, string | number>) =>
  values ? `${key}(${Object.values(values).join("|")})` : key;

const CONNECTIONS = [
  { id: "__local__", label: "Local" },
  { id: "c2", label: "NAS" },
];

const TASK: ScheduleTask = {
  id: "t1",
  name: "nightly",
  kind: "copy",
  sourceConnectionId: "__local__",
  sourcePath: "/data",
  targetConnectionId: "c2",
  targetPath: "/mirror",
  cron: "*/15 * * * *",
  enabled: false,
  options: { backupDir: "_backups", verifyAfter: true, retentionDays: 7 },
  bisyncResyncDone: true,
  createdAt: 1,
  lastRunAt: null,
  lastRunStatus: null,
  nextRunAt: null,
};

function mountDialog(task: ScheduleTask | null = null) {
  return mount(ScheduleDialog, {
    props: { t, locale: "en", task, connections: CONNECTIONS },
    attachTo: document.body,
  });
}

describe("ScheduleDialog", () => {
  it("defaults to a create draft on the first connection and refuses empty submits", async () => {
    const wrapper = mountDialog();
    const confirm = wrapper.get(".wb-dialog-primary");
    expect((confirm.element as HTMLButtonElement).disabled).toBe(true);
    await wrapper.get('input[aria-label="scheduleNameLabel"]').setValue("nightly");
    await wrapper.get('input[aria-label="scheduleTargetLabel"]').setValue("/mirror");
    expect((wrapper.get(".wb-dialog-primary").element as HTMLButtonElement).disabled).toBe(false);
  });

  it("rejects an invalid cron and accepts a valid one with a preview", async () => {
    const wrapper = mountDialog();
    await wrapper.get('input[aria-label="scheduleNameLabel"]').setValue("task");
    await wrapper.get('input[aria-label="scheduleTargetLabel"]').setValue("/mirror");
    const cronInput = wrapper.get('input[aria-label="scheduleCronLabel"]');
    await cronInput.setValue("not a cron");
    expect(wrapper.text()).toContain("scheduleCronInvalid");
    expect((wrapper.get(".wb-dialog-primary").element as HTMLButtonElement).disabled).toBe(true);
    await cronInput.setValue("*/5 * * * *");
    expect(wrapper.text()).toContain("scheduleNextRunPreview");
  });

  it("blocks a same-connection same-path pair", async () => {
    const wrapper = mountDialog();
    await wrapper.get('input[aria-label="scheduleNameLabel"]').setValue("task");
    await wrapper.get('input[aria-label="syncSourceLabel"]').setValue("/same");
    await wrapper.get('input[aria-label="scheduleTargetLabel"]').setValue("/same");
    expect(wrapper.text()).toContain("destMustDiffer");
    expect((wrapper.get(".wb-dialog-primary").element as HTMLButtonElement).disabled).toBe(true);
  });

  it("assembles the confirm payload with option mapping", async () => {
    const wrapper = mountDialog();
    await wrapper.get('input[aria-label="scheduleNameLabel"]').setValue("nightly");
    await wrapper.get('input[aria-label="syncSourceLabel"]').setValue("/data");
    await wrapper.get('input[aria-label="scheduleTargetLabel"]').setValue("/mirror");
    // 高级区:backupDir + suffix + 保留天数 + verifyAfter + exclude 过滤。
    await wrapper.get("details summary").trigger("click");
    const advanced = wrapper.get("details");
    await advanced.findAll("input")[0].setValue("_backups");
    await advanced.findAll("input")[1].setValue(".bak");
    await advanced.findAll("input")[2].setValue("14");
    await advanced.findAll("input")[3].setValue("2");
    await advanced.findAll("input")[4].setValue("");
    await advanced.findAll("input")[5].setValue("*.log");
    await advanced.findAll("input")[6].setValue(true as unknown as string);
    await wrapper.get(".wb-dialog-primary").trigger("click");
    const [draft] = wrapper.emitted("confirm")![0] as unknown as Array<Record<string, unknown>>;
    expect(draft).toMatchObject({
      name: "nightly",
      kind: "sync",
      sourceConnectionId: "__local__",
      sourcePath: "/data",
      targetConnectionId: "__local__",
      targetPath: "/mirror",
      enabled: true,
    });
    const options = draft.options as Record<string, unknown>;
    expect(options).toMatchObject({
      backupDir: "_backups",
      suffix: ".bak",
      retentionDays: 14,
      maxDelete: 2,
      exclude: ["*.log"],
      verifyAfter: true,
    });
    expect(options.include).toBeUndefined();
    expect(options.dryRun).toBeUndefined();
  });

  it("prefills an existing task for editing", () => {
    const wrapper = mountDialog(TASK);
    expect((wrapper.get('input[aria-label="scheduleNameLabel"]').element as HTMLInputElement).value).toBe("nightly");
    expect((wrapper.get('input[aria-label="scheduleCronLabel"]').element as HTMLInputElement).value).toBe("*/15 * * * *");
    expect((wrapper.get('input[type="checkbox"]').element as HTMLInputElement).checked).toBe(false);
  });

  it("keeps the target connection in sync with the source until touched", async () => {
    const wrapper = mountDialog();
    const selects = wrapper.findAll("select");
    await selects[1].setValue("c2"); // 目标连接
    await selects[0].setValue("c2"); // 切换源连接
    expect((selects[1].element as HTMLSelectElement).value).toBe("c2");
  });
});
