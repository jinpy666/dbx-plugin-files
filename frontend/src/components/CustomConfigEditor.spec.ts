// @vitest-environment happy-dom
// CustomConfigEditor（rclone-custom 连接编辑器）组件级回归：
// service 可输入（datalist 建议 rclone 后端全集，手输可达任意类型）、
// JSON 参数草稿、换后端播种示例草稿、connection/test 按钮状态机与
// notice/error 事件形态。真正建连接走宿主的连接表单（manifest 定义）。
import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";

vi.mock("../lib/api", () => ({
  callLifecycle: vi.fn(),
  errorMessage: (cause: unknown) => (cause instanceof Error ? cause.message : String(cause)),
}));

import { callLifecycle } from "../lib/api";
import CustomConfigEditor from "./CustomConfigEditor.vue";

const call = vi.mocked(callLifecycle);

function mountEditor() {
  return mount(CustomEditor, { props: { t: (key: string) => key } });
}
// 别名仅为本文件可读性；mount 统一走 mountEditor。
const CustomEditor = CustomConfigEditor;

/** service 输入框（rclone 透传可达任意后端类型，datalist 建议非白名单）。 */
function serviceInput(wrapper: ReturnType<typeof mountEditor>) {
  return wrapper.find("#custom-service-input");
}

async function chooseService(wrapper: ReturnType<typeof mountEditor>, value: string) {
  await serviceInput(wrapper).setValue(value);
  // 手输场景下 change 在失焦/回车时触发，与 datalist 点选保持同一路径。
  await serviceInput(wrapper).trigger("change");
}

beforeEach(() => {
  call.mockReset();
  call.mockResolvedValue({});
});

describe("CustomConfigEditor", () => {
  it("opens empty with the rclone backend datalist and a JSON draft", () => {
    const wrapper = mountEditor();
    expect((serviceInput(wrapper).element as HTMLInputElement).value).toBe("");
    // datalist 建议覆盖 rclone 后端全集。
    const suggestions = wrapper
      .findAll("#custom-service-options option")
      .map((option) => option.attributes("value"));
    expect(suggestions).toContain("b2");
    expect(suggestions).toContain("mega");
    expect(wrapper.text()).toContain("customConfigHint");
    expect(wrapper.find("textarea").element.value).toBe("{}");
  });

  it("tests a typed backend type through the host lifecycle with the rclone-custom protocol", async () => {
    const wrapper = mountEditor();
    await chooseService(wrapper, "b2");
    await wrapper.find("textarea").setValue('{ "account": "a", "key": "k" }');
    await wrapper.find(".wb-toolbar-button").trigger("click");
    await flushPromises();
    expect(call).toHaveBeenCalledTimes(1);
    const [method, params] = call.mock.calls[0];
    expect(method).toBe("connection/test");
    const external = (params as { connection: { external_config: Record<string, unknown> } })
      .connection.external_config;
    expect(external.protocol).toBe("rclone-custom");
    expect(external.service).toBe("b2");
    expect(external.config).toEqual({ account: "a", key: "k" });
    expect(wrapper.text()).toContain("customConfigTestOk");
    expect(wrapper.emitted("notice")?.at(-1)?.[0]).toMatchObject({ key: "customConfigTestOk" });
  });

  it("seeds the JSON draft from the known-backend hint and follows untouched drafts", async () => {
    const wrapper = mountEditor();
    await chooseService(wrapper, "b2");
    expect(JSON.parse(wrapper.find("textarea").element.value)).toEqual({ account: "...", key: "..." });
    // 草稿仍是自动播种内容（用户未编辑）时，换后端跟随重新播种。
    await chooseService(wrapper, "mega");
    expect(JSON.parse(wrapper.find("textarea").element.value)).toEqual({ user: "...", pass: "..." });
    // 用户编辑过的草稿不被换后端覆盖。
    await wrapper.find("textarea").setValue('{ "user": "me" }');
    await chooseService(wrapper, "http");
    expect(wrapper.find("textarea").element.value).toBe('{ "user": "me" }');
  });

  it("rejects non-object JSON before calling the lifecycle", async () => {
    const wrapper = mountEditor();
    await wrapper.find("textarea").setValue("[1, 2]");
    await wrapper.find(".wb-toolbar-button").trigger("click");
    expect(call).not.toHaveBeenCalled();
    const error = wrapper.emitted("error")?.at(-1)?.[0];
    expect(error).toMatchObject({ key: "customConfigInvalid" });
  });

  it("reports failures through the error channel and keeps the button usable", async () => {
    call.mockRejectedValueOnce(new Error("boom"));
    const wrapper = mountEditor();
    await chooseService(wrapper, "b2");
    await wrapper.find(".wb-toolbar-button").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("customConfigTestFailed");
    expect(wrapper.emitted("error")?.at(-1)?.[0]).toMatchObject({ key: "customConfigTestFailed" });
    expect(wrapper.find(".wb-toolbar-button").attributes("disabled")).toBeUndefined();
  });

  it("disables the test button while a test is in flight", async () => {
    let release!: (value: Record<string, unknown>) => void;
    call.mockReturnValueOnce(new Promise((resolve) => (release = resolve)));
    const wrapper = mountEditor();
    await wrapper.find(".wb-toolbar-button").trigger("click");
    expect(wrapper.find(".wb-toolbar-button").attributes("disabled")).toBeDefined();
    release({});
    await flushPromises();
    expect(wrapper.find(".wb-toolbar-button").attributes("disabled")).toBeUndefined();
  });

  it("clears the last result when the service changes", async () => {
    call.mockRejectedValueOnce(new Error("boom"));
    const wrapper = mountEditor();
    await chooseService(wrapper, "b2");
    await wrapper.find(".wb-toolbar-button").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("customConfigTestFailed");
    await chooseService(wrapper, "mega");
    expect(wrapper.text()).not.toContain("customConfigTestFailed");
  });
});
