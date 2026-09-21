// @vitest-environment happy-dom
// PathField 交互：点层级跳转；点面包屑空白处直接进入编辑态（不再必须点左侧图标）。
import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import PathField from "./PathField.vue";
import { vTip } from "../lib/tooltip";

const t = (key: string) => key;

function field() {
  return mount(PathField, {
    props: { path: "/logs/openssh", maxVisible: 4, t },
    global: { directives: { tip: vTip } },
  });
}

describe("PathField", () => {
  it("enters edit mode when the blank crumb area is clicked", async () => {
    const wrapper = field();
    expect(wrapper.find(".wb-path-input").exists()).toBe(false);
    await wrapper.get(".wb-path-crumb-area").trigger("click");
    expect(wrapper.find(".wb-path-input").exists()).toBe(true);
    expect((wrapper.get(".wb-path-input").element as HTMLInputElement).value).toBe("/logs/openssh");
    wrapper.unmount();
  });

  it("still navigates (and keeps display mode) when a crumb level is clicked", async () => {
    const wrapper = field();
    await wrapper.get(".wb-path-crumb-area").find("button").trigger("click");
    expect(wrapper.find(".wb-path-input").exists()).toBe(false);
    expect(wrapper.emitted("navigate")?.length).toBeGreaterThan(0);
    wrapper.unmount();
  });
});
