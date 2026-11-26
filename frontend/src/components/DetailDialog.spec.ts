// @vitest-environment happy-dom
// 文件详情对话框（issue #98 只读先行）组件级冒烟：基础区/ETag/metadata
// 表/POSIX 区按 detail 内容有则显示，无扩展元数据时优雅降级提示；
// Esc 与关闭按钮都走 close 事件，关闭态不渲染。
import { afterEach, describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { nextTick } from "vue";
import DetailDialog from "./DetailDialog.vue";

const t = (key: string, values?: Record<string, string | number>) => key;

const baseEntry = {
  name: "a.png",
  path: "/photos/a.png",
  kind: "file" as const,
  size: 1024,
  modifiedAt: "2026-11-19T10:00:00Z",
};

const s3Detail = {
  mimeType: "image/png",
  etag: '"9af3b0f2"',
  metadata: [
    { key: "cache-control", value: "max-age=3600" },
    { key: "content-type", value: "image/png" },
  ],
};
function mountDialog(props: Record<string, unknown> = {}) {
  return mount(DetailDialog, {
    attachTo: document.body,
    props: { open: true, entry: baseEntry, detail: s3Detail, t, ...props },
  });
}

async function settle(wrapper: VueWrapper) {
  await wrapper.vm.$nextTick();
  await nextTick();
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("DetailDialog", () => {
  it("renders nothing while closed", () => {
    const wrapper = mountDialog({ open: false });
    expect(wrapper.find('[role="dialog"]').exists()).toBe(false);
  });

  it("renders basic rows with formatted size and time", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    const dialog = wrapper.find('[role="dialog"]');
    expect(dialog.exists()).toBe(true);
    expect(dialog.classes()).toContain("wb-detail");
    expect(dialog.text()).toContain("detailTitle");
    expect(dialog.text()).toContain("a.png");
    expect(dialog.text()).toContain("/photos/a.png");
    expect(dialog.text()).toContain("1.0 KiB");
    expect(dialog.text()).toContain("2026-");
  });

  it("shows etag and metadata table for object storage entries", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    const dialog = wrapper.find('[role="dialog"]');
    expect(dialog.text()).toContain('"9af3b0f2"');
    expect(dialog.text()).toContain("detailEtag");
    const rows = dialog.findAll(".wb-detail-meta-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].text()).toContain("cache-control");
    expect(rows[1].text()).toContain("content-type");
  });

  it("shows the posix block when metadata carries mode/uid/gid", async () => {
    const wrapper = mountDialog({
      entry: { ...baseEntry, name: "data", path: "/srv/data", kind: "directory", size: undefined },
      detail: { mode: "0755", uid: "0", gid: "0" },
    });
    await settle(wrapper);
    const dialog = wrapper.find('[role="dialog"]');
    const posix = dialog.find(".wb-detail-posix");
    expect(posix.exists()).toBe(true);
    expect(posix.text()).toContain("0755");
    expect(posix.text()).toContain("0");
    expect(dialog.findAll(".wb-detail-meta-row")).toHaveLength(0);
  });

  it("shows checksum hashes when the backend reports them", async () => {
    const wrapper = mountDialog({
      detail: { mimeType: "text/plain", hashes: { md5: "2b21731b43c4af152cea3b6ee1d2aabf", sha1: "f572d396" } },
    });
    await settle(wrapper);
    const dialog = wrapper.find('[role="dialog"]');
    expect(dialog.text()).toContain("detailHashes");
    const rows = dialog.findAll(".wb-detail-hash-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].text()).toContain("md5");
    expect(rows[0].text()).toContain("2b21731b43c4af152cea3b6ee1d2aabf");
  });

  it("degrades with a hint when the backend offers no extended metadata", async () => {
    const wrapper = mountDialog({ detail: { mimeType: "text/plain" } });
    await settle(wrapper);
    const dialog = wrapper.find('[role="dialog"]');
    expect(dialog.text()).toContain("detailNoMetadata");
    expect(dialog.find(".wb-detail-posix").exists()).toBe(false);
    expect(dialog.findAll(".wb-detail-meta-row")).toHaveLength(0);
    expect(dialog.text()).not.toContain("detailEtag");
  });

  it("emits close on the close button and on Escape", async () => {
    const wrapper = mountDialog();
    await settle(wrapper);
    await wrapper.find(".wb-detail-close").trigger("click");
    expect(wrapper.emitted("close")).toHaveLength(1);
    await wrapper.find('[role="dialog"]').trigger("keydown", { key: "Escape" });
    expect(wrapper.emitted("close")).toHaveLength(2);
  });
});

// issue #100：S3 metadata 编辑入口只在后端声明能力时出现；提交前本地校验
// 与父层 saveState 驱动的退出编辑态。
const editableDetail = {
  mimeType: "text/plain",
  metadata: [
    { key: "content-type", value: "text/plain" },
    { key: "x-amz-meta-owner", value: "alice" },
  ],
};

describe("DetailDialog metadata editing (#100)", () => {
  it("hides the edit affordance without backend capability", async () => {
    const wrapper = mountDialog({ detail: editableDetail });
    await settle(wrapper);
    expect(wrapper.find('[data-test="metadata-edit"]').exists()).toBe(false);
  });

  it("hides the edit affordance for directories", async () => {
    const wrapper = mountDialog({
      detail: editableDetail,
      canEditMetadata: true,
      entry: { ...baseEntry, name: "dir", path: "/photos", kind: "directory", size: undefined },
    });
    await settle(wrapper);
    expect(wrapper.find('[data-test="metadata-edit"]').exists()).toBe(false);
  });

  it("seeds the editor and emits the wire map on save", async () => {
    const wrapper = mountDialog({ detail: editableDetail, canEditMetadata: true });
    await settle(wrapper);
    await wrapper.find('[data-test="metadata-edit"]').trigger("click");
    await settle(wrapper);

    const editor = wrapper.find('[data-test="metadata-editor"]');
    expect(editor.exists()).toBe(true);
    expect(
      (wrapper.find('[data-test="metadata-content-type"]').element as HTMLInputElement).value,
    ).toBe("text/plain");
    const keys = wrapper.findAll('[data-test="metadata-key"]');
    expect(keys).toHaveLength(1);
    expect((keys[0]!.element as HTMLInputElement).value).toBe("owner");

    await wrapper.find('[data-test="metadata-content-type"]').setValue("application/json");
    await wrapper.find('[data-test="metadata-value"]').setValue("bob");
    await wrapper.find('[data-test="metadata-save"]').trigger("click");

    expect(wrapper.emitted("save")).toEqual([
      [{ "content-type": "application/json", "x-amz-meta-owner": "bob" }],
    ]);
  });

  it("blocks empty values locally and shows a translated hint", async () => {
    const wrapper = mountDialog({ detail: editableDetail, canEditMetadata: true });
    await settle(wrapper);
    await wrapper.find('[data-test="metadata-edit"]').trigger("click");
    await settle(wrapper);
    await wrapper.find('[data-test="metadata-value"]').setValue("");
    await wrapper.find('[data-test="metadata-save"]').trigger("click");
    await settle(wrapper);

    expect(wrapper.emitted("save")).toBeUndefined();
    expect(wrapper.find('[data-test="metadata-error"]').text()).toBe("metadataEmptyValue");
  });

  it("leaves the editor when the parent reports a saved state", async () => {
    const wrapper = mountDialog({ detail: editableDetail, canEditMetadata: true });
    await settle(wrapper);
    await wrapper.find('[data-test="metadata-edit"]').trigger("click");
    await settle(wrapper);
    expect(wrapper.find('[data-test="metadata-editor"]').exists()).toBe(true);

    await wrapper.setProps({ saveState: "saved" });
    await settle(wrapper);
    expect(wrapper.find('[data-test="metadata-editor"]').exists()).toBe(false);
  });

  it("Escape exits the editor without closing the dialog", async () => {
    const wrapper = mountDialog({ detail: editableDetail, canEditMetadata: true });
    await settle(wrapper);
    await wrapper.find('[data-test="metadata-edit"]').trigger("click");
    await settle(wrapper);

    await wrapper.find('[role="dialog"]').trigger("keydown", { key: "Escape" });
    await settle(wrapper);
    expect(wrapper.find('[data-test="metadata-editor"]').exists()).toBe(false);
    expect(wrapper.emitted("close")).toBeUndefined();
    expect(wrapper.find('[role="dialog"]').exists()).toBe(true);
  });
});
