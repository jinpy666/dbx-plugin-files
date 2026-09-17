import { describe, expect, it } from "vitest";
import { fileIconKind } from "./fileIcons";

describe("fileIconKind 按扩展名分类", () => {
  it.each([
    ["photo.png", "image"],
    ["snapshot.JPEG", "image"],
    ["报告.docx", "word"],
    ["预算表.xlsx", "excel"],
    ["data.csv", "excel"],
    ["路演.pptx", "ppt"],
    ["manual.pdf", "pdf"],
    ["backup.tar.gz", "archive"],
    ["release.zip", "archive"],
    ["app.vue", "code"],
    ["config.yaml", "code"],
    ["song.flac", "audio"],
    ["clip.mp4", "video"],
    ["notes.txt", "file"],
    ["Makefile", "file"],
    [".gitignore", "file"],
    ["noext.", "file"],
  ])("%s → %s", (name, expected) => {
    expect(fileIconKind(name)).toBe(expected);
  });
});
