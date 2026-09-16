// @vitest-environment happy-dom
// CustomConfigEditor（opendal-custom 连接编辑器）组件级回归：
// Form/JSON 双模切换守卫、表单校验门控（必填/安全类别）、密码掩码、
// connection/test 按钮状态机与 notice/error 事件形态。
// lib 层（opendalServices.ts）的 schema/校验/往返同步已有专门 spec；
// 这里固定的是组件把 lib 行为呈现给用户的体验路径。
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

/** s3 schema 的文本输入顺序：bucket, endpoint, region, access_key_id, secret_access_key(password)。 */
const S3_FIELDS = ["bucket", "endpoint", "region", "access_key_id", "secret_access_key"] as const;

async function mountS3() {
  const wrapper = mountEditor();
  await wrapper.find("select").setValue("s3");
  return wrapper;
}

function textInputs(wrapper: ReturnType<typeof mountEditor>) {
  return wrapper.findAll("input").filter((input) => input.attributes("type") !== "checkbox");
}

function fillS3(wrapper: ReturnType<typeof mountEditor>, values: Partial<Record<(typeof S3_FIELDS)[number], string>>) {
  const inputs = textInputs(wrapper);
  for (const [index, key] of S3_FIELDS.entries()) {
    if (values[key] !== undefined) void inputs[index].setValue(values[key]);
  }
}

function tabButton(wrapper: ReturnType<typeof mountEditor>, key: string) {
  return wrapper.findAll(".wb-pane-tabs button").find((button) => button.text() === key);
}

beforeEach(() => {
  call.mockReset();
  call.mockResolvedValue({});
});

describe("CustomConfigEditor", () => {
  it("opens on fs with the Form tab and a root text field", () => {
    const wrapper = mountEditor();
    expect((wrapper.find("select").element as HTMLSelectElement).value).toBe("fs");
    expect(wrapper.findAll("input")).toHaveLength(1);
    expect(textInputs(wrapper)[0].attributes("placeholder")).toBe("empty = whole filesystem");
  });

  it("switching service seeds the JSON hint and hydrates the form (MinIO loopback hint included)", async () => {
    const wrapper = await mountS3();
    // 现状固定：空草稿时用服务提示填充。s3 提示里是 http://127.0.0.1:9000，
    // 而 Form 模式的安全校验会拒绝环回地址 —— 见下方 loopback 门控用例与报告。
    const endpoint = textInputs(wrapper)[1].element as HTMLInputElement;
    expect(endpoint.value).toBe("http://127.0.0.1:9000");
  });

  it("syncs form edits into the JSON draft (form → JSON direction)", async () => {
    const wrapper = await mountS3();
    fillS3(wrapper, { bucket: "demo-bucket" });
    // Form 模式下草稿只在组件状态里，切到 JSON tab 才落 DOM
    await tabButton(wrapper, "customJsonTab")!.trigger("click");
    const draft = JSON.parse(wrapper.find("textarea").element.value) as Record<string, unknown>;
    expect(draft.bucket).toBe("demo-bucket");
  });

  it("hydrates form inputs from the JSON draft (JSON → form direction)", async () => {
    const wrapper = await mountS3();
    await tabButton(wrapper, "customJsonTab")!.trigger("click");
    await wrapper.find("textarea").setValue('{ "bucket": "from-json", "customKey": 1 }');
    await tabButton(wrapper, "customFormTab")!.trigger("click");
    expect((textInputs(wrapper)[0].element as HTMLInputElement).value).toBe("from-json");
  });

  it("blocks the form switch on invalid JSON and stays in JSON mode with the error hint", async () => {
    const wrapper = await mountS3();
    await tabButton(wrapper, "customJsonTab")!.trigger("click");
    await wrapper.find("textarea").setValue("{not json");
    await tabButton(wrapper, "customFormTab")!.trigger("click");
    expect(wrapper.text()).toContain("customJsonInvalidSwitch");
    // 留在 JSON 模式：表单字段不渲染，textarea 仍在
    expect(textInputs(wrapper)).toHaveLength(0);
    expect(wrapper.find("textarea").exists()).toBe(true);
  });

  it("masks credential inputs as type=password", async () => {
    const wrapper = await mountS3();
    const secret = textInputs(wrapper)[4];
    expect(secret.attributes("type")).toBe("password");
  });

  it("refuses the test when a required field is empty and never calls lifecycle", async () => {
    const wrapper = await mountS3();
    fillS3(wrapper, {
      bucket: "demo",
      endpoint: "https://s3.amazonaws.com",
      // 服务提示预填了 "..." 占位，显式清空才能触发必填校验
      access_key_id: "",
      secret_access_key: "",
    });
    await wrapper.find(".wb-toolbar-button").trigger("click");
    expect(call).not.toHaveBeenCalled();
    const error = wrapper.emitted("error")?.at(-1)?.[0];
    expect(error).toMatchObject({ key: "customFieldRequiredError" });
  });

  it("refuses the test on loopback/private endpoints (UI guard; JSON mode and backend stay permissive)", async () => {
    const wrapper = await mountS3();
    fillS3(wrapper, {
      bucket: "demo",
      // 保持提示自带的环回端点：Form 模式拒绝，后端 policy 对用户显式端点放行
      endpoint: "http://127.0.0.1:9000",
      access_key_id: "ak",
      secret_access_key: "sk",
    });
    await wrapper.find(".wb-toolbar-button").trigger("click");
    expect(call).not.toHaveBeenCalled();
    const error = wrapper.emitted("error")?.at(-1)?.[0];
    expect(error).toMatchObject({ key: "customFieldHost" });
  });

  it("tests a valid form config through the host lifecycle and reports ok", async () => {
    const wrapper = await mountS3();
    fillS3(wrapper, {
      bucket: "demo",
      endpoint: "https://s3.amazonaws.com",
      access_key_id: "ak",
      secret_access_key: "sk",
    });
    await wrapper.find(".wb-toolbar-button").trigger("click");
    await flushPromises();
    expect(call).toHaveBeenCalledTimes(1);
    const [method, params] = call.mock.calls[0];
    expect(method).toBe("connection/test");
    const external = (params as { connection: { external_config: Record<string, unknown> } }).connection.external_config;
    expect(external.protocol).toBe("opendal-custom");
    expect(external.service).toBe("s3");
    expect(external.config).toMatchObject({ bucket: "demo", access_key_id: "ak", secret_access_key: "sk" });
    expect(wrapper.text()).toContain("customConfigTestOk");
    expect(wrapper.emitted("notice")?.at(-1)?.[0]).toMatchObject({ key: "customConfigTestOk" });
  });

  it("reports failures through the error channel and keeps the button usable", async () => {
    call.mockRejectedValueOnce(new Error("boom"));
    const wrapper = await mountS3();
    fillS3(wrapper, {
      bucket: "demo",
      endpoint: "https://s3.amazonaws.com",
      access_key_id: "ak",
      secret_access_key: "sk",
    });
    await wrapper.find(".wb-toolbar-button").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("customConfigTestFailed");
    expect(wrapper.emitted("error")?.at(-1)?.[0]).toMatchObject({ key: "customConfigTestFailed" });
    expect(wrapper.find(".wb-toolbar-button").attributes("disabled")).toBeUndefined();
  });

  it("disables the test button while a test is in flight", async () => {
    let release!: (value: Record<string, unknown>) => void;
    call.mockReturnValueOnce(new Promise((resolve) => (release = resolve)));
    const wrapper = await mountS3();
    fillS3(wrapper, {
      bucket: "demo",
      endpoint: "https://s3.amazonaws.com",
      access_key_id: "ak",
      secret_access_key: "sk",
    });
    await wrapper.find(".wb-toolbar-button").trigger("click");
    expect(wrapper.find(".wb-toolbar-button").attributes("disabled")).toBeDefined();
    release({});
    await flushPromises();
    expect(wrapper.find(".wb-toolbar-button").attributes("disabled")).toBeUndefined();
  });

  it("clears the last result when the service changes", async () => {
    call.mockRejectedValueOnce(new Error("boom"));
    const wrapper = await mountS3();
    fillS3(wrapper, {
      bucket: "demo",
      endpoint: "https://s3.amazonaws.com",
      access_key_id: "ak",
      secret_access_key: "sk",
    });
    await wrapper.find(".wb-toolbar-button").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("customConfigTestFailed");
    await wrapper.find("select").setValue("fs");
    expect(wrapper.text()).not.toContain("customConfigTestFailed");
  });
});
