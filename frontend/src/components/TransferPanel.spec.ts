// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import TransferPanel from "./TransferPanel.vue";
import type { TransferJob } from "../lib/transfers";

// R5-P2-3 回归：计数型 job（批量删除伪 job 的 transferred/size 实为文件个数）
// 进度必须按「N/M 项」渲染，不得再走 formatBytes 把 10000 个文件显示成 9.8 KiB。

function mountPanel(jobs: TransferJob[]) {
  return mount(TransferPanel, {
    props: {
      jobs,
      // i18n stub：filesProgress 键按模板展开，便于断言 done/total 数值
      t: (key: string, values?: Record<string, string | number>) =>
        key === "filesProgress" ? `${values?.done}/${values?.total} files` : key,
    },
  });
}

describe("TransferPanel progressMeta for count-based jobs (R5-P2-3)", () => {
  it("delete pseudo-job renders N/M files instead of bytes", () => {
    const job: TransferJob = {
      jobId: "local-batch-1",
      connectionId: "c",
      kind: "delete",
      state: "running",
      size: 10000,
      transferred: 504,
      filesDone: 504,
      filesTotal: 10000,
      updatedAt: Date.now(),
    };
    const wrapper = mountPanel([job]);
    const meta = wrapper.find(".wb-transfer-meta").text();
    expect(meta).toContain("504/10000 files");
    expect(meta).not.toContain("KiB");
    expect(meta).not.toContain("B /");
  });

  it("plain byte-based single-file upload still renders bytes", () => {
    const job: TransferJob = {
      jobId: "task-1",
      connectionId: "c",
      kind: "upload",
      state: "running",
      size: 2048,
      transferred: 1024,
      updatedAt: Date.now(),
    };
    const wrapper = mountPanel([job]);
    const meta = wrapper.find(".wb-transfer-meta").text();
    expect(meta).toContain("1.0 KiB / 2.0 KiB");
  });
});
