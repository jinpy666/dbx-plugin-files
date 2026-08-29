import { describe, expect, it } from "vitest";
import { canEditBytes, hexDump, imageMimeFor, READ_MAX_BYTES, WRITE_MAX_BYTES } from "./preview";

describe("preview helpers", () => {
  it("enforces the files/write size limit for edits", () => {
    expect(WRITE_MAX_BYTES).toBe(4 * 1024 * 1024);
    expect(READ_MAX_BYTES).toBe(2 * 1024 * 1024);
    expect(canEditBytes(0)).toBe(true);
    expect(canEditBytes(WRITE_MAX_BYTES)).toBe(true);
    expect(canEditBytes(WRITE_MAX_BYTES + 1)).toBe(false);
  });

  it("maps image extensions to MIME types", () => {
    expect(imageMimeFor("/a.png")).toBe("image/png");
    expect(imageMimeFor("/a.JPG")).toBe("image/jpeg");
    expect(imageMimeFor("/logo.svg")).toBe("image/svg+xml");
    expect(imageMimeFor("/noext")).toBeNull();
    expect(imageMimeFor("/a.exe")).toBeNull();
  });

  it("renders hex dump rows with offset, hex and ascii columns", () => {
    const bytes = new Uint8Array([0x48, 0x65, 0x00, 0x7f]);
    const dump = hexDump(bytes);
    const lines = dump.split("\n");
    expect(lines).toHaveLength(1);
    expect(lines[0]).toContain("00000000");
    expect(lines[0]).toContain("48 65");
    expect(lines[0]).toContain("7f");
    expect(lines[0]).toContain("|He");
  });

  it("pads incomplete rows and renders multiple rows at 16 bytes", () => {
    const bytes = new Uint8Array(20).fill(0x41);
    const lines = hexDump(bytes).split("\n");
    expect(lines).toHaveLength(2);
    // 第二行只有 4 字节有效：4 个 hex "41"，ASCII 列为纯字符 "AAAA"
    expect(lines[1].startsWith("00000010")).toBe(true);
    expect(lines[1].match(/41/g)?.length).toBe(4);
    expect(lines[1]).toContain("|AAAA|");
  });
});
