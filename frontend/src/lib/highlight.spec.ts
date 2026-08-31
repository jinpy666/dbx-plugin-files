import { describe, expect, it } from "vitest";
import { extensionOf, highlightCode } from "./highlight";

describe("extensionOf", () => {
  it("extracts lowercase extension from bare names and paths", () => {
    expect(extensionOf("main.rs")).toBe("rs");
    expect(extensionOf("/a/b/APP.TS")).toBe("ts");
    expect(extensionOf("archive.tar.gz")).toBe("gz");
  });

  it("returns empty for dotfiles and extensionless names", () => {
    expect(extensionOf(".gitignore")).toBe("");
    expect(extensionOf("Makefile")).toBe("");
  });
});

describe("highlightCode", () => {
  it("highlights known languages into escaped HTML", () => {
    const html = highlightCode('const n = "x";', "app.js");
    expect(html).toContain("hljs-keyword");
    expect(html).toContain("hljs-string");
  });

  it("maps json and falls back by path basename", () => {
    expect(highlightCode('{"a":1}', "/tmp/data.json")).toContain("hljs-attr");
  });

  it("returns null for unknown extensions and dotfiles", () => {
    expect(highlightCode("anything", "data.bin")).toBeNull();
    expect(highlightCode("anything", ".gitignore")).toBeNull();
  });

  it("never throws on odd input", () => {
    expect(highlightCode("\u0000\uFFFF", "weird.py")).not.toBeNull();
  });
});
