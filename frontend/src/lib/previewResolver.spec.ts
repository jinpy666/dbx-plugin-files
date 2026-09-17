import { describe, expect, it } from "vitest";
import { resolvePreview } from "./previewResolver";

describe("resolvePreview", () => {
  it("resolves images as read-only file-viewer previews", () => {
    expect(resolvePreview("/assets/Logo.PNG")).toEqual({
      extension: "png",
      mime: "image/png",
      kind: "image",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
      fallbackReason: null,
    });
  });

  it("resolves known text and code files as editable CodeMirror files", () => {
    expect(resolvePreview("/src/main.TS")).toEqual({
      extension: "ts",
      mime: "text/typescript",
      kind: "text",
      editable: true,
      previewStrategy: "file-viewer",
      editorStrategy: "codemirror",
      fallback: false,
      fallbackReason: null,
    });

    expect(resolvePreview("/docs/readme.md")).toMatchObject({
      extension: "md",
      mime: "text/markdown",
      kind: "text",
      editable: true,
      editorStrategy: "codemirror",
      fallback: false,
    });
  });

  it("recognizes archives without making them editable", () => {
    expect(resolvePreview("/backups/site.tar.GZ")).toEqual({
      extension: "tar.gz",
      mime: "application/gzip",
      kind: "archive",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
      fallbackReason: null,
    });

    expect(resolvePreview("/backups/archive.zip")).toMatchObject({
      extension: "zip",
      mime: "application/zip",
      kind: "archive",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
    });
  });

  it("resolves Office documents and PDF as read-only file-viewer previews", () => {
    expect(resolvePreview("/docs/report.docx")).toMatchObject({
      extension: "docx",
      mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
      kind: "office",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
    });
    expect(resolvePreview("/docs/report.pdf")).toMatchObject({
      extension: "pdf",
      mime: "application/pdf",
      kind: "pdf",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
    });
  });

  it("resolves audio and video as read-only media", () => {
    expect(resolvePreview("/media/intro.mp4")).toMatchObject({
      extension: "mp4",
      mime: "video/mp4",
      kind: "media",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
    });
    expect(resolvePreview("/media/song.ogg")).toMatchObject({
      extension: "ogg",
      mime: "audio/ogg",
      kind: "media",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: false,
    });
  });

  it("keeps unknown extensions on file-viewer with an explicit fallback marker", () => {
    expect(resolvePreview("/downloads/archive.weird")).toEqual({
      extension: "weird",
      mime: null,
      kind: "unknown",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: true,
      fallbackReason: "unknown-extension",
    });
  });

  it("handles paths without extensions and mixed path casing", () => {
    expect(resolvePreview("/README")).toMatchObject({
      extension: "",
      mime: null,
      kind: "unknown",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
      fallback: true,
      fallbackReason: "unknown-extension",
    });
    expect(resolvePreview("/tmp/.env")).toMatchObject({
      extension: "",
      kind: "unknown",
      fallback: true,
    });
  });

  it("does not expose an editor strategy for read-only categories", () => {
    for (const path of ["/a.png", "/a.pdf", "/a.mp3", "/a.zip", "/a.bin", "/a.docx"]) {
      const result = resolvePreview(path);
      expect(result.previewStrategy).toBe("file-viewer");
      expect(result.editable).toBe(false);
      expect(result.editorStrategy).toBeNull();
    }
  });
});
