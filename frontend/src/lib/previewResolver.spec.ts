import { describe, expect, it } from "vitest";
import { resolvePreview } from "./previewResolver";

describe("resolvePreview", () => {
  it("resolves images to the native image element", () => {
    expect(resolvePreview("/assets/Logo.PNG")).toEqual({
      extension: "png",
      mime: "image/png",
      kind: "image",
      editable: false,
      previewStrategy: "image",
      editorStrategy: null,
      fallback: false,
      fallbackReason: null,
    });
  });

  it("keeps known text and code on CodeMirror for preview and editing", () => {
    expect(resolvePreview("/src/main.TS")).toEqual({
      extension: "ts",
      mime: "text/typescript",
      kind: "text",
      editable: true,
      previewStrategy: "codemirror",
      editorStrategy: "codemirror",
      fallback: false,
      fallbackReason: null,
    });

    expect(resolvePreview("/docs/readme.md")).toMatchObject({
      extension: "md",
      mime: "text/markdown",
      kind: "text",
      editable: true,
      previewStrategy: "codemirror",
      editorStrategy: "codemirror",
      fallback: false,
    });
  });

  it("recognizes a wide range of programming languages as editable text", () => {
    for (const [path, extension] of [
      ["/code/main.go", "go"],
      ["/code/server.php", "php"],
      ["/code/Program.cs", "cs"],
      ["/code/Main.kt", "kt"],
      ["/code/App.swift", "swift"],
      ["/code/main.dart", "dart"],
      ["/code/hello.scala", "scala"],
      ["/code/fib.hs", "hs"],
      ["/code/script.lua", "lua"],
      ["/code/tool.pl", "pl"],
      ["/code/deploy.ps1", "ps1"],
      ["/code/Job.groovy", "groovy"],
      ["/code/schema.proto", "proto"],
      ["/code/render.m", "m"],
      ["/code/style.scss", "scss"],
      ["/code/fix.diff", "diff"],
      ["/code/fix.patch", "patch"],
      ["/code/site.conf", "conf"],
    ] as const) {
      const result = resolvePreview(path);
      expect(result, path).toMatchObject({
        extension,
        kind: "text",
        editable: true,
        previewStrategy: "codemirror",
        editorStrategy: "codemirror",
      });
    }
  });

  it("recognizes archives without making them editable", () => {
    expect(resolvePreview("/backups/site.tar.GZ")).toEqual({
      extension: "tar.gz",
      mime: "application/gzip",
      kind: "archive",
      editable: false,
      previewStrategy: "archive",
      editorStrategy: null,
      fallback: false,
      fallbackReason: null,
    });

    expect(resolvePreview("/backups/archive.zip")).toMatchObject({
      extension: "zip",
      mime: "application/zip",
      kind: "archive",
      editable: false,
      previewStrategy: "archive",
      editorStrategy: null,
      fallback: false,
    });
  });

  it("delegates Office documents and PDF to the file viewer", () => {
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

  it("routes CSV/TSV to the viewer but keeps HTML on CodeMirror (dark-theme safe)", () => {
    expect(resolvePreview("/data/data.csv")).toMatchObject({
      extension: "csv",
      kind: "text",
      editable: false,
      previewStrategy: "file-viewer",
      editorStrategy: null,
    });
    expect(resolvePreview("/site/page.html")).toMatchObject({
      extension: "html",
      kind: "text",
      editable: true,
      previewStrategy: "codemirror",
      editorStrategy: "codemirror",
    });
    expect(resolvePreview("/data/table.TSV")).toMatchObject({
      extension: "tsv",
      previewStrategy: "file-viewer",
    });
  });

  it("resolves audio and video as viewer media", () => {
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

  it("keeps unknown extensions on the text/hex heuristic with a fallback marker", () => {
    expect(resolvePreview("/downloads/archive.weird")).toEqual({
      extension: "weird",
      mime: null,
      kind: "unknown",
      editable: false,
      previewStrategy: "hex",
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
      previewStrategy: "hex",
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

  it("splits rendering tiers across the expected strategies", () => {
    expect(resolvePreview("/a.png").previewStrategy).toBe("image");
    expect(resolvePreview("/a.ts").previewStrategy).toBe("codemirror");
    expect(resolvePreview("/a.zip").previewStrategy).toBe("archive");
    expect(resolvePreview("/a.pdf").previewStrategy).toBe("file-viewer");
    expect(resolvePreview("/a.docx").previewStrategy).toBe("file-viewer");
    expect(resolvePreview("/a.mp3").previewStrategy).toBe("file-viewer");
    expect(resolvePreview("/a.bin").previewStrategy).toBe("hex");
  });

  it("does not expose an editor strategy for read-only categories", () => {
    for (const path of ["/a.png", "/a.pdf", "/a.mp3", "/a.zip", "/a.bin", "/a.docx", "/a.csv"]) {
      const result = resolvePreview(path);
      expect(result.editable).toBe(false);
      expect(result.editorStrategy).toBeNull();
    }
  });
});
