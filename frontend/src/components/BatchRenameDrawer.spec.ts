// @vitest-environment happy-dom
// 批量重命名抽屉 UI 单测：计划表实时预览、行级错误渲染、应用逐行 files/rename
// （沿用单文件 rename 调用链）、applied 汇总与关闭。
import { afterEach, describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { bindApi } from "../lib/api";
import BatchRenameDrawer from "./BatchRenameDrawer.vue";
import type { FileEntry } from "../lib/api";

let wrapper: VueWrapper | undefined;

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
});

const t = (key: string, values?: Record<string, string | number>) =>
  values?.count !== undefined ? `${key}:${values.count}` : key;

function entry(name: string): FileEntry {
  return { name, path: `/dir/${name}`, kind: "file", size: 1 };
}

function mountDrawer(props: Partial<InstanceType<typeof BatchRenameDrawer>["$props"]> = {}) {
  wrapper = mount(BatchRenameDrawer, {
    props: {
      entries: [entry("a.txt"), entry("b.txt")],
      siblingNames: ["a.txt", "b.txt"],
      connectionId: "conn-1",
      t,
      ...props,
    },
    attachTo: document.body,
  });
  return wrapper;
}

async function flush(times = 6) {
  for (let i = 0; i < times; i += 1) await Promise.resolve();
}

function inputOf(root: VueWrapper, test: string) {
  return root.get(`[data-test=${test}]`);
}

describe("BatchRenameDrawer", () => {
  it("renders the live plan with prefix/suffix/numbering transformations", async () => {
    mountDrawer();
    await inputOf(wrapper!, "prefix").setValue("pre-");
    await inputOf(wrapper!, "suffix").setValue("-bak");
    await inputOf(wrapper!, "numbering").setValue(true);
    await inputOf(wrapper!, "numbering-start").setValue("3");
    const rows = wrapper!.findAll("[data-test=plan-row]");
    expect(rows.map((row) => row.text())).toEqual([
      expect.stringContaining("pre-a.txt-bak-003"),
      expect.stringContaining("pre-b.txt-bak-004"),
    ]);
    expect(wrapper!.get("[data-test=apply]").text()).toContain("batchRenameApply:2");
  });

  it("shows row-level errors for duplicates and directory-existing names", async () => {
    mountDrawer({ entries: [entry("a.txt"), entry("b.txt")], siblingNames: ["a.txt", "b.txt", "c.txt"] });
    // a/b → 都变 x.txt：第二行计划重复；再验证改为 c.txt 时命中目录已存在。
    await inputOf(wrapper!, "find").setValue("a|b");
    await inputOf(wrapper!, "replace").setValue("x");
    await inputOf(wrapper!, "regex").setValue(true);
    expect(wrapper!.findAll("[data-test=plan-row]")[1].classes()).toContain("is-error");
    expect(wrapper!.findAll("[data-test=plan-row]")[1].text()).toContain("batchRenameErrorDuplicate");

    await inputOf(wrapper!, "find").setValue("a");
    await inputOf(wrapper!, "replace").setValue("c");
    expect(wrapper!.findAll("[data-test=plan-row]")[0].text()).toContain("batchRenameErrorExists");
    // 全部行带错误：应用不可用。
    expect((wrapper!.get("[data-test=apply]").element as HTMLButtonElement).disabled).toBe(true);
  });

  it("applies sequential renames per changed row and reports the summary", async () => {
    const renames: Array<Record<string, unknown>> = [];
    bindApi(async <T,>(method: string, params: unknown) => {
      if (method === "files/rename") {
        renames.push(params as Record<string, unknown>);
        return { success: true, transport: "native", jobId: null } as unknown as T;
      }
      throw new Error(`unexpected ${method}`);
    }, null);
    mountDrawer();
    await inputOf(wrapper!, "find").setValue("a");
    await inputOf(wrapper!, "replace").setValue("z");
    await wrapper!.get("[data-test=apply]").trigger("click");
    await flush();
    // 只有计划发生变化的行会下发 rename；路径沿用原父目录。
    expect(renames).toEqual([
      { connectionId: "conn-1", path: "/dir/a.txt", newPath: "/dir/z.txt" },
    ]);
    expect(wrapper!.emitted("applied")).toEqual([[{ ok: 1, total: 1 }]]);
    expect(wrapper!.findAll("[data-test=plan-row]")[0].text()).toContain("batchRenameOk");
  });

  it("keeps the drawer open with the per-row failure when a rename fails", async () => {
    let fail = true;
    bindApi(async <T,>(method: string) => {
      if (method === "files/rename") {
        if (fail) throw new Error("backend refused");
        return { success: true } as unknown as T;
      }
      throw new Error(`unexpected ${method}`);
    }, null);
    mountDrawer();
    await inputOf(wrapper!, "find").setValue("a");
    await inputOf(wrapper!, "replace").setValue("z");
    await wrapper!.get("[data-test=apply]").trigger("click");
    await flush();
    expect(wrapper!.findAll("[data-test=plan-row]")[0].text()).toContain("backend refused");
    expect(wrapper!.emitted("applied")).toEqual([[{ ok: 0, total: 1 }]]);
    fail = false;
  });
});
