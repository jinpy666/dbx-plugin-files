// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { workbenchMessage } from "./i18n";
import { friendlyError, isNotFoundMessage, isTransportFailure } from "./friendlyError";

const t = (key: string, values?: Record<string, string | number>) => workbenchMessage("zh-CN", key, values);

describe("friendlyError", () => {
  it("maps known error classes to localized text", () => {
    expect(friendlyError("NotFound: /docs/gone", t)).toBe("目标不存在或已被删除");
    expect(friendlyError("open /etc/hosts: permission denied", t)).toBe("没有操作权限");
    expect(friendlyError("Archive target 'a.tar.gz' already exists", t)).toBe("目标已存在");
    expect(friendlyError("dial tcp: connection refused", t)).toBe("存储连接失败或超时");
  });

  it("keeps unknown messages verbatim (fallback to raw)", () => {
    expect(friendlyError("mock backend failure for /error-dir", t)).toBe("mock backend failure for /error-dir");
  });

  it("is locale aware", () => {
    const en = (key: string) => workbenchMessage("en", key);
    expect(friendlyError("NotFound: /x", en)).toBe("The target does not exist or has been removed");
  });
});

describe("isTransportFailure", () => {
  it("matches network/timeout failures only", () => {
    expect(isTransportFailure("dial tcp 1.2.3.4:443: i/o timeout")).toBe(true);
    expect(isTransportFailure("connection reset by peer")).toBe(true);
    expect(isTransportFailure("NotFound: /docs")).toBe(false);
    expect(isTransportFailure("mock backend failure for /error-dir")).toBe(false);
    expect(isTransportFailure("Method not found: files/list")).toBe(false);
  });
});

describe("isNotFoundMessage", () => {
  it("detects missing-target business errors", () => {
    expect(isNotFoundMessage("NotFound: /docs/x")).toBe(true);
    expect(isNotFoundMessage("no such file or directory")).toBe(true);
    expect(isNotFoundMessage("permission denied")).toBe(false);
  });
});
