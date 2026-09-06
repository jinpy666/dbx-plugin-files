// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Breadcrumbs from "./Breadcrumbs.vue";

/** 面包屑可视文本（含分隔符），用于断言无双斜杠形态。 */
function visibleText(wrapper: ReturnType<typeof mount>): string {
  return wrapper.find("nav").text();
}

describe("Breadcrumbs (P1-1 根段无双斜杠)", () => {
  it("renders root without a leading double slash", () => {
    const wrapper = mount(Breadcrumbs, { props: { path: "/Users/demo" } });
    const buttons = wrapper.findAll("button").map((b) => b.text());
    expect(buttons).toEqual(["/", "Users", "demo"]);
    // 分隔符只出现在非根段之间：/Users/demo（1 个 sep），而非 //Users/demo
    expect(wrapper.findAll(".wb-crumb-sep")).toHaveLength(1);
    expect(visibleText(wrapper)).toBe("/Users/demo");
    expect(visibleText(wrapper)).not.toContain("//");
  });

  it("renders a bare root path as a single crumb", () => {
    const wrapper = mount(Breadcrumbs, { props: { path: "/" } });
    expect(wrapper.findAll("button")).toHaveLength(1);
    expect(wrapper.findAll(".wb-crumb-sep")).toHaveLength(0);
  });

  it("keeps collapsed state free of double slash after the root (deep path)", async () => {
    const wrapper = mount(Breadcrumbs, { props: { path: "/a/b/c/d/e/f", maxVisible: 4 } });
    // 折叠态：/ … /d /e /f —— 根后是省略号，不得出现 //…
    const buttons = wrapper.findAll("button").map((b) => b.text());
    expect(buttons).toEqual(["/", "…", "d", "e", "f"]);
    expect(wrapper.findAll(".wb-crumb-sep")).toHaveLength(3);
    expect(visibleText(wrapper)).not.toContain("//");
    // 点击省略号展开后同样无双斜杠
    await wrapper.find(".wb-crumb-ellipsis").trigger("click");
    expect(wrapper.findAll("button").map((b) => b.text())).toEqual(["/", "a", "b", "c", "d", "e", "f"]);
    expect(visibleText(wrapper)).not.toContain("//");
  });

  it("emits navigable paths on crumb click", async () => {
    const wrapper = mount(Breadcrumbs, { props: { path: "/a/b" } });
    await wrapper.findAll("button")[1]!.trigger("click");
    expect(wrapper.emitted("navigate")?.[0]).toEqual(["/a"]);
  });
});
