// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { workbenchMessage } from "./i18n";
import { friendlyError, isConnectionNotReadyMessage, isNotFoundMessage, isTransportFailure } from "./friendlyError";

const t = (key: string, values?: Record<string, string | number>) => workbenchMessage("zh-CN", key, values);

describe("friendlyError", () => {
  it("maps known error classes to localized text", () => {
    expect(friendlyError("NotFound: /docs/gone", t)).toBe("目标不存在或已被删除。请刷新目录并检查路径。");
    expect(friendlyError("open /etc/hosts: permission denied", t)).toBe("没有操作权限。请检查连接凭据、访问权限和只读设置。");
    expect(friendlyError("Archive target 'a.tar.gz' already exists", t)).toBe("目标已存在。请更换名称或目标路径。");
    expect(friendlyError("dial tcp: connection refused", t)).toBe("存储连接失败或超时。请检查服务器地址和网络后重新连接。");
  });

  it("maps unregistered-connection engine errors to actionable guidance", () => {
    // binding() 的 registry miss：双栏跨连接选择器指向未 connect 的宿主连接。
    expect(friendlyError("Connection is not connected (rclone engine)", t)).toBe(
      "该连接尚未在后台建立:请关闭本页签,然后在 DBX 左侧连接列表重新打开该连接。",
    );
    // mock/MCP 路由的 registry miss 同类文案。
    expect(friendlyError("Unknown connectionId 'x'; connect first", t)).toBe(
      "该连接尚未在后台建立:请关闭本页签,然后在 DBX 左侧连接列表重新打开该连接。",
    );
    // 宿主侧/dev-host 的同族 registry miss（#144 恢复自愈的暂时态）。
    expect(friendlyError("Connection is not active; reopen it from DBX", t)).toBe(
      "该连接尚未在后台建立:请关闭本页签,然后在 DBX 左侧连接列表重新打开该连接。",
    );
    expect(friendlyError("Connection not found", t)).toBe(
      "该连接尚未在后台建立:请关闭本页签,然后在 DBX 左侧连接列表重新打开该连接。",
    );
  });

  it("keeps unknown messages verbatim (fallback to raw)", () => {
    expect(friendlyError("mock backend failure for /error-dir", t)).toBe("mock backend failure for /error-dir");
  });

  it("is locale aware", () => {
    const en = (key: string) => workbenchMessage("en", key);
    expect(friendlyError("NotFound: /x", en)).toBe("The target does not exist or has been removed. Refresh the folder and check the path.");
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

  it("matches rclone rcd cold-start transport errors as transient", () => {
    // E2E 实证（web 容器重启后恢复页首拉）：rcd respawn 窗口的失败形状。
    expect(isTransportFailure("rc transport error: error sending request for url (http://127.0.0.1:44235/config/create) [connection closed before message completed]")).toBe(true);
  });
});

describe("isNotFoundMessage", () => {
  it("detects missing-target business errors", () => {
    expect(isNotFoundMessage("NotFound: /docs/x")).toBe(true);
    expect(isNotFoundMessage("no such file or directory")).toBe(true);
    expect(isNotFoundMessage("permission denied")).toBe(false);
  });
});

describe("isConnectionNotReadyMessage", () => {
  it("detects the registry-miss engine error and not network failures", () => {
    expect(isConnectionNotReadyMessage("Connection is not connected (rclone engine)")).toBe(true);
    expect(isConnectionNotReadyMessage("dial tcp: connection refused")).toBe(false);
    expect(isConnectionNotReadyMessage("NotFound: /x")).toBe(false);
  });

  it("detects host-side inactive/not-found registry misses as transient", () => {
    // ssh 引擎同款 + dev-host 文案：boot 恢复自愈窗口必须把它们当暂时态。
    expect(isConnectionNotReadyMessage("Connection is not active; reopen it from DBX")).toBe(true);
    expect(isConnectionNotReadyMessage("connection is not active")).toBe(true);
    expect(isConnectionNotReadyMessage("Connection not found")).toBe(true);
    expect(isConnectionNotReadyMessage("Plugin backend is not running")).toBe(false);
  });
});

