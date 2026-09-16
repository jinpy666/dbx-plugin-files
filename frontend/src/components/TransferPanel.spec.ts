// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount, config } from "@vue/test-utils";
import TransferPanel from "./TransferPanel.vue";
import type { TransferJob } from "../lib/transfers";
import { splitTransferPath } from "../lib/transfers";
import { vTip } from "../lib/tooltip";

// 模板里的 v-tip（图标按钮提示）在测试挂载时同样需要指令注册。
config.global.directives = { tip: vTip };

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

describe("transfer path presentation", () => {
  it("keeps the final name separate from a truncated parent path", () => {
    expect(splitTransferPath("/Users/me/Library/Application Support/DBX/very-long-folder/report.pdf")).toEqual({
      name: "report.pdf",
      parent: "/Users/me/Library/Application Support/DBX/very-long-folder",
    });
  });

  it("preserves root-level names without inventing a parent", () => {
    expect(splitTransferPath("report.pdf")).toEqual({ name: "report.pdf", parent: "" });
  });
});

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

// 本机落盘下载的行级交互（对标 ssh 面板：定位/打开 + 单条删除）。

describe("TransferPanel status icons", () => {
  const states: TransferJob["state"][] = ["queued", "running", "completed", "failed", "canceled"];

  it.each(states)("renders the %s icon before the file name without visible state text", (state) => {
    const wrapper = mountPanel([{
      jobId: `job-${state}`,
      connectionId: "c",
      kind: "download",
      state,
      size: 10,
      transferred: state === "completed" ? 10 : 0,
      updatedAt: Date.now(),
    }]);
    const title = wrapper.find(".wb-transfer-title");
    const icon = title.find(".wb-transfer-status-icon");
    const strong = title.find("strong");
    expect(icon.exists()).toBe(true);
    expect(icon.element.compareDocumentPosition(strong.element) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(icon.classes()).toContain(`is-${state}`);
    expect(icon.attributes("aria-label")).toBe(`transferStatus.${state}`);
    expect(title.find(".wb-transfer-state").exists()).toBe(false);
  });
});

describe("TransferPanel history ordering", () => {
  it("shows the newest completed transfer first by finishedAt", () => {
    const wrapper = mountPanel([
      {
        jobId: "older",
        connectionId: "c",
        kind: "upload",
        state: "completed",
        size: 1,
        transferred: 1,
        remotePath: "/older.txt",
        updatedAt: 900,
        createdAt: 100,
        finishedAt: 900,
      },
      {
        jobId: "newer",
        connectionId: "c",
        kind: "upload",
        state: "completed",
        size: 1,
        transferred: 1,
        remotePath: "/newer.txt",
        updatedAt: 1,
        createdAt: 200,
        finishedAt: 1,
      },
    ]);
    expect(wrapper.findAll(".wb-transfer-item strong").map((item) => item.text())).toEqual(["newer.txt", "older.txt"]);
  });
});

describe("TransferPanel record interactions", () => {
  function completedDownload(): TransferJob {
    return {
      jobId: "dl-1",
      connectionId: "c",
      kind: "download",
      state: "completed",
      size: 4096,
      transferred: 4096,
      localPath: "/Users/me/Downloads/report.pdf",
      updatedAt: Date.now(),
    };
  }

  it("active rows show the parent path below the emphasized final name", () => {
    const wrapper = mountPanel([{
      jobId: "active-1",
      connectionId: "c",
      kind: "upload",
      state: "running",
      size: 10,
      transferred: 2,
      remotePath: "/var/lib/dbx/very-long-folder/report.pdf",
      updatedAt: Date.now(),
    }]);
    const item = wrapper.find(".wb-transfer-item");
    expect(item.find(".wb-transfer-title strong").text()).toBe("report.pdf");
    expect(item.find(".wb-transfer-path-parent").text()).toBe("/var/lib/dbx/very-long-folder");
  });

  it("history rows with a localPath expose reveal/open buttons and the recorded path", async () => {
    const wrapper = mountPanel([completedDownload()]);
    const path = wrapper.find(".wb-transfer-localpath");
    expect(path.exists()).toBe(true);
    expect(path.find(".wb-transfer-path-name").text()).toBe("report.pdf");
    expect(path.find(".wb-transfer-path-parent").text()).toBe("/Users/me/Downloads");
    expect(path.attributes("title")).toBe("/Users/me/Downloads/report.pdf");
    const buttons = wrapper.findAll(".wb-transfer-item button");
    const tips = buttons.map((button) => button.attributes("aria-label") ?? button.text());
    expect(tips).toContain("revealInFolder");
    expect(tips).toContain("openDownloadedFile");
    await buttons.find((button) => (button.attributes("aria-label") ?? "") === "revealInFolder")!.trigger("click");
    await buttons.find((button) => (button.attributes("aria-label") ?? "") === "openDownloadedFile")!.trigger("click");
    expect(wrapper.emitted("reveal")).toEqual([["/Users/me/Downloads/report.pdf"]]);
    expect(wrapper.emitted("open")).toEqual([["/Users/me/Downloads/report.pdf"]]);
  });

  it("every history row offers single-record delete and emits the jobId", async () => {
    const wrapper = mountPanel([completedDownload()]);
    const buttons = wrapper.findAll(".wb-transfer-item button");
    const deleteButton = buttons.find((button) => (button.attributes("aria-label") ?? "") === "deleteRecord");
    expect(deleteButton).toBeTruthy();
    await deleteButton!.trigger("click");
    expect(wrapper.emitted("delete")).toEqual([["dl-1"]]);
  });

  it("rows without a localPath hide reveal/open but still allow delete", () => {
    const wrapper = mountPanel([
      { jobId: "up-1", connectionId: "c", kind: "upload", state: "failed", size: 10, transferred: 0, updatedAt: Date.now() },
    ]);
    const tips = wrapper.findAll(".wb-transfer-item button").map((button) => button.attributes("aria-label"));
    expect(tips).not.toContain("revealInFolder");
    expect(tips).not.toContain("openDownloadedFile");
    expect(tips).toContain("deleteRecord");
  });

  it("does not render the download directory setting in the transfer panel", () => {
    const wrapper = mountPanel([completedDownload()]);
    expect(wrapper.find(".wb-transfer-savedir").exists()).toBe(false);
  });
});
