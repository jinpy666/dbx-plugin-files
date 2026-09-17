// @vitest-environment happy-dom
import { defineComponent, h, nextTick } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";

const { preset, viewerProps } = vi.hoisted(() => ({
  preset: { name: "all-renderers" },
  viewerProps: { current: null as Record<string, unknown> | null },
}));

vi.mock("@file-viewer/vue3", () => ({
  FileViewer: defineComponent({
    name: "MockFileViewer",
    inheritAttrs: false,
    props: ["file", "url", "name", "filename", "type", "size", "options"],
    setup(props: Record<string, unknown>, { emit }) {
      viewerProps.current = props;
      return () => h("div", { "data-test": "file-viewer", ...props, onError: (error: unknown) => emit("error", error), onUnsupported: (error: unknown) => emit("unsupported", error) });
    },
  }),
}));
vi.mock("@file-viewer/preset-all", () => ({ default: preset, allRenderers: preset }));

import FileViewerPreview from "./FileViewerPreview.vue";

type Source = Blob | File | string;

let wrapper: VueWrapper | undefined;

afterEach(() => {
  wrapper?.unmount();
  wrapper = undefined;
  viewerProps.current = null;
  vi.restoreAllMocks();
});

function mountPreview(source: Source, extra: Record<string, unknown> = {}) {
  wrapper = mount(FileViewerPreview, { props: { source, ...extra } });
  return wrapper;
}

describe("FileViewerPreview", () => {
  it("passes a named File and full preset options for Blob sources", () => {
    const blob = new Blob(["hello"], { type: "text/plain" });
    mountPreview(blob, { fileName: "notes.txt", mime: "text/custom", options: { locale: "zh-CN" } });

    expect(viewerProps.current?.file).toBeInstanceOf(File);
    expect((viewerProps.current?.file as File).name).toBe("notes.txt");
    expect((viewerProps.current?.file as File).type).toBe("text/custom");
    expect(viewerProps.current?.url).toMatch(/^blob:/);
    expect(viewerProps.current?.name).toBe("notes.txt");
    expect(viewerProps.current?.filename).toBe("notes.txt");
    expect(viewerProps.current?.type).toBe("text/custom");
    expect(viewerProps.current?.size).toBe(blob.size);
    expect(viewerProps.current?.options).toEqual({ locale: "zh-CN", rendererMode: "replace", preset });
  });

  it("forwards external URLs without revoking them", async () => {
    const revoke = vi.spyOn(URL, "revokeObjectURL");
    const url = "https://example.test/report.pdf";
    const mounted = mountPreview(url, { fileName: "report.pdf", mime: "application/pdf" });

    expect(viewerProps.current).toMatchObject({ url, name: "report.pdf", filename: "report.pdf", type: "application/pdf" });
    mounted.unmount();
    expect(revoke).not.toHaveBeenCalled();
    wrapper = undefined;
  });

  it("revokes an internally created URL when source changes and on unmount", async () => {
    const create = vi.spyOn(URL, "createObjectURL").mockReturnValueOnce("blob:first").mockReturnValueOnce("blob:second");
    const revoke = vi.spyOn(URL, "revokeObjectURL");
    const mounted = mountPreview(new Blob(["one"]), { fileName: "one.txt" });
    await nextTick();
    await mounted.setProps({ source: new Blob(["two"]), fileName: "two.txt" });
    expect(create).toHaveBeenCalledTimes(2);
    expect(revoke).toHaveBeenCalledWith("blob:first");
    mounted.unmount();
    expect(revoke).toHaveBeenLastCalledWith("blob:second");
    wrapper = undefined;
  });

  it("maps lifecycle completion and error events to adapter events", async () => {
    const loaded = vi.fn();
    const unloaded = vi.fn();
    const failed = vi.fn();
    const mounted = mountPreview("https://example.test/file.txt", { onLoad: loaded, onUnload: unloaded, onError: failed });
    const child = mounted.findComponent({ name: "MockFileViewer" });
    child.vm.$emit("load-complete", { phase: "load-complete" });
    child.vm.$emit("unload-complete", { phase: "unload-complete" });
    child.vm.$emit("error", new Error("failed"));
    await nextTick();
    expect(loaded).toHaveBeenCalledWith({ phase: "load-complete" });
    expect(unloaded).toHaveBeenCalledWith({ phase: "unload-complete" });
    expect(failed).toHaveBeenCalledWith(expect.any(Error));
  });
});
