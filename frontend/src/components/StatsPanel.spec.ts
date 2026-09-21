// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import StatsPanel from "./StatsPanel.vue";
import type { TransferJob } from "../lib/transfers";

const t = (key: string) => key;

function job(overrides: Partial<TransferJob>): TransferJob {
  return {
    jobId: overrides.jobId ?? "job",
    connectionId: "conn",
    kind: "upload",
    state: "queued",
    size: 0,
    transferred: 0,
    updatedAt: 0,
    ...overrides,
  };
}

const jobs: TransferJob[] = [
  job({ jobId: "run", state: "running", rateBps: 2048 }),
  job({ jobId: "run2", state: "running", rateBps: 1024 }),
  job({ jobId: "queue", state: "queued" }),
  job({ jobId: "done", state: "completed", transferred: 500 }),
  job({ jobId: "bad", state: "failed", transferred: 100, error: "boom" }),
];

function mountPanel(overrides: { jobs?: TransferJob[]; usage?: { used: number; total: number } | null; bwlimit?: string | null } = {}) {
  return mount(StatsPanel, {
    props: {
      jobs: overrides.jobs ?? jobs,
      usage: overrides.usage !== undefined ? overrides.usage : { used: 25, total: 100 },
      bwlimit: overrides.bwlimit !== undefined ? overrides.bwlimit : "10M",
      t,
    },
  });
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = "";
});

describe("StatsPanel（统计页签）", () => {
  it("summarizes job states and cumulative bytes from the tracker", () => {
    const wrapper = mountPanel();
    const cells = wrapper.findAll(".wb-stats-cell");
    // 活动含 queued+running；累计字节数只计历史任务（500 + 100）。
    expect(cells[0].text()).toContain("statsActiveTransfers");
    expect(cells[0].find("strong").text()).toBe("3");
    expect(cells[1].find("strong").text()).toBe("1");
    expect(cells[2].find("strong").text()).toBe("1");
    expect(cells[2].classes()).toContain("is-bad");
    expect(cells[3].text()).toContain("600 B");
    wrapper.unmount();
  });

  it("shows the active bandwidth limit (限速状态行)", () => {
    const wrapper = mountPanel();
    expect(wrapper.find(".wb-stats-limit").text()).toBe("10M");
    expect(wrapper.find(".wb-stats-limit").classes()).toContain("is-on");
    wrapper.unmount();
  });

  it("renders the throughput curve after two samples and marks the current rate", async () => {
    const wrapper = mountPanel();
    // 挂载即采第一个样；两个样后曲线成形（单样只显示空态）。
    expect(wrapper.find(".wb-stats-chart").exists()).toBe(false);
    expect(wrapper.find(".wb-stats-chart-empty").exists()).toBe(true);
    vi.advanceTimersByTime(2100);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(".wb-stats-chart").exists()).toBe(true);
    // 当前速率 = 运行中 job rateBps 之和（2048 + 1024 = 3072 → 3.0 KiB/s）。
    expect(wrapper.find(".wb-stats-rate").text()).toContain("3.0 KiB");
    wrapper.unmount();
  });

  it("renders storage usage as a percent bar with used/total", () => {
    const wrapper = mountPanel();
    expect(wrapper.find(".wb-progress").exists()).toBe(true);
    expect(wrapper.find(".wb-progress").attributes("aria-valuenow")).toBe("25");
    expect(wrapper.find(".wb-stats-storage-meta").text()).toContain("25 B");
    expect(wrapper.find(".wb-stats-storage-meta").text()).toContain("100 B");
    wrapper.unmount();
  });

  it("falls back to a hint when the backend reports no capacity", () => {
    const wrapper = mountPanel({ usage: null });
    expect(wrapper.find(".wb-progress").exists()).toBe(false);
    expect(wrapper.find(".wb-stats-storage-meta").text()).toBe("statsStorageUnavailable");
    wrapper.unmount();
  });

  it("shows unlimited for the limit cell when no rate applies", () => {
    const wrapper = mountPanel({ bwlimit: null });
    expect(wrapper.find(".wb-stats-limit").text()).toBe("bwlimitUnlimited");
    wrapper.unmount();
  });
});
