import { describe, expect, it } from "vitest";
import type { FileEntry } from "./api";
import { decodeDisplayName, withDisplayName } from "./charset";

describe("decodeDisplayName", () => {
  it("解码 rclone ‛XX GBK 转义序列", () => {
    // "中文" 的 GBK 字节 D6 D0 CE C4。
    expect(decodeDisplayName("‛D6‛D0‛CE‛C4.txt", "gbk")).toBe("中文.txt");
  });

  it("混合段：转义与明文共存", () => {
    expect(decodeDisplayName("pkg/‛D6‛D0/readme", "gbk")).toBe("pkg/中/readme");
  });

  it("‛‛ 还原为字面 ‛", () => {
    expect(decodeDisplayName("a‛‛z", "gbk")).toBe("a‛z");
  });

  it("无转义或未配置字符集时原样返回", () => {
    const raw = "plain.bin";
    expect(decodeDisplayName(raw, "gbk")).toBe(raw);
    const escaped = "‛D6‛D0.bin";
    expect(decodeDisplayName(escaped, "")).toBe(escaped);
    expect(decodeDisplayName(escaped, "not-a-charset")).toBe(escaped);
  });

  it("解码失败回退原始转义文本", () => {
    // 0xFC 单字节无法构成合法 GBK 序列 → 回退原始转义。
    const raw = "‛FC.bin";
    expect(decodeDisplayName(raw, "gbk")).toBe(raw);
  });
});

describe("withDisplayName", () => {
  it("产生差异时附加 displayName", () => {
    const entry: FileEntry = { name: "‛D6‛D0.txt", path: "/‛D6‛D0.txt", kind: "file" };
    const next = withDisplayName(entry, "gbk");
    expect(next.displayName).toBe("中.txt");
    expect(next.name).toBe(entry.name);
    expect(next.path).toBe(entry.path);
  });

  it("无差异时不附加 displayName", () => {
    const entry: FileEntry = { name: "plain.txt", path: "/plain.txt", kind: "file" };
    expect(withDisplayName(entry, "gbk")).toBe(entry);
  });
});
